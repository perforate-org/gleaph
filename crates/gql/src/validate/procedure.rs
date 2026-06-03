use rapidhash::RapidHashSet;

use crate::ast::*;

use super::{VResult, verr};

/// Validates that YIELD item output names (alias or bare name) are unique.
pub(super) fn validate_yield_alias_uniqueness(yields: &[YieldItem], context: &str) -> VResult {
    let mut seen = RapidHashSet::default();
    for item in yields {
        let output_name = item.alias.as_ref().unwrap_or(&item.name);
        if !seen.insert(output_name.clone()) {
            return Err(verr(&format!(
                "{context}: duplicate output name '{output_name}'"
            )));
        }
    }
    Ok(())
}

/// Validates a named CALL procedure statement.
pub(super) fn validate_call_procedure(cp: &CallProcedureStatement) -> VResult {
    if cp.name.parts.is_empty() {
        return Err(verr("CALL procedure name must not be empty"));
    }
    if let Some(ref yields) = cp.yield_items {
        validate_yield_alias_uniqueness(yields, "CALL YIELD")?;
    }
    Ok(())
}

/// Validates an inline procedure call (scope variable duplicates, body).
pub(super) fn validate_inline_scope_vars(ipc: &InlineProcedureCall) -> VResult {
    let mut seen = RapidHashSet::default();
    if let InlineProcedureScope::Explicit(vars) = &ipc.scope {
        for var in vars {
            if !seen.insert(var.clone()) {
                return Err(verr(&format!(
                    "inline CALL: duplicate scope variable '{var}'"
                )));
            }
        }
    }
    Ok(())
}
