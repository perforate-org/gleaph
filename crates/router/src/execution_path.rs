//! GQL program kind vs IC call kind (query / update) validation.

use gleaph_gql::program_modification::ProgramModificationFlags;
use gleaph_graph_kernel::plan_exec::GqlExecutionMode;

use crate::state::RouterError;

const REMEDY_WRITE_ON_QUERY: &str =
    "use gql_execute_idempotent (update call with client_mutation_key)";
const REMEDY_READ_ON_UPDATE: &str = "use gql_query (composite query call) or force_gql_execute";
const REMEDY_PREPARED_WRITE_ON_QUERY: &str =
    "use prepared_execute_update_idempotent (update call with client_mutation_key)";
const REMEDY_PREPARED_READ_ON_UPDATE: &str =
    "use prepared_execute_query (composite query call) or force_prepared_execute_update";

/// Returns whether the program requires the update (write) canister path.
pub fn program_requires_write_path(flags: ProgramModificationFlags) -> bool {
    flags.requires_write_path()
}

/// Reject when `mode` and parsed program flags disagree (unless `force`).
pub fn check_execution_path(
    entrypoint: &str,
    mode: GqlExecutionMode,
    requires_write_path: bool,
    force: bool,
    remedy_read_on_update: &'static str,
    remedy_write_on_query: &'static str,
) -> Result<(), RouterError> {
    if force {
        return Ok(());
    }
    let program_kind = if requires_write_path {
        "write"
    } else {
        "read-only"
    };
    match (mode, requires_write_path) {
        (GqlExecutionMode::Query, true) => Err(RouterError::ExecutionPathMismatch {
            entrypoint: entrypoint.to_string(),
            program_kind: program_kind.to_string(),
            call_kind: "query".to_string(),
            remedy: remedy_write_on_query.to_string(),
        }),
        (GqlExecutionMode::Update, false) => Err(RouterError::ExecutionPathMismatch {
            entrypoint: entrypoint.to_string(),
            program_kind: program_kind.to_string(),
            call_kind: "update".to_string(),
            remedy: remedy_read_on_update.to_string(),
        }),
        _ => Ok(()),
    }
}

pub fn check_adhoc_execution_path(
    entrypoint: &str,
    mode: GqlExecutionMode,
    flags: ProgramModificationFlags,
    force: bool,
) -> Result<(), RouterError> {
    check_execution_path(
        entrypoint,
        mode,
        program_requires_write_path(flags),
        force,
        REMEDY_READ_ON_UPDATE,
        REMEDY_WRITE_ON_QUERY,
    )
}

pub fn check_prepared_execution_path(
    entrypoint: &str,
    mode: GqlExecutionMode,
    requires_write_path: bool,
    force: bool,
) -> Result<(), RouterError> {
    check_execution_path(
        entrypoint,
        mode,
        requires_write_path,
        force,
        REMEDY_PREPARED_READ_ON_UPDATE,
        REMEDY_PREPARED_WRITE_ON_QUERY,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::program_modification::ProgramModificationFlags;

    #[test]
    fn rejects_write_program_on_query_call() {
        let flags = ProgramModificationFlags {
            has_data_modification: true,
            ..Default::default()
        };
        let err = check_adhoc_execution_path("gql_query", GqlExecutionMode::Query, flags, false)
            .expect_err("write on query");
        assert!(matches!(
            err,
            RouterError::ExecutionPathMismatch {
                entrypoint,
                program_kind,
                call_kind,
                ..
            } if entrypoint == "gql_query"
                && program_kind == "write"
                && call_kind == "query"
        ));
    }

    #[test]
    fn rejects_read_program_on_update_call() {
        let err = check_adhoc_execution_path(
            "gql_execute",
            GqlExecutionMode::Update,
            ProgramModificationFlags::default(),
            false,
        )
        .expect_err("read on update");
        assert!(matches!(
            err,
            RouterError::ExecutionPathMismatch { call_kind, .. } if call_kind == "update"
        ));
    }

    #[test]
    fn force_allows_read_on_update() {
        check_adhoc_execution_path(
            "force_gql_execute",
            GqlExecutionMode::Update,
            ProgramModificationFlags::default(),
            true,
        )
        .expect("force bypass");
    }

    #[test]
    fn prepared_rejects_write_plan_on_query_call() {
        let err = check_prepared_execution_path(
            "prepared_execute_query",
            GqlExecutionMode::Query,
            true,
            false,
        )
        .expect_err("write on query");
        assert!(matches!(
            err,
            RouterError::ExecutionPathMismatch {
                entrypoint,
                program_kind,
                call_kind,
                ..
            } if entrypoint == "prepared_execute_query"
                && program_kind == "write"
                && call_kind == "query"
        ));
    }

    #[test]
    fn prepared_force_bypasses_read_on_update_mismatch() {
        check_prepared_execution_path(
            "force_prepared_execute_update",
            GqlExecutionMode::Update,
            false,
            true,
        )
        .expect("force bypass");
    }
}
