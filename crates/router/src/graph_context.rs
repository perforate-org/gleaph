//! Resolve effective graph context from a GQL program (ADR 0011 §1).

use gleaph_gql::ast::{GqlProgram, SessionCommand, SessionResetCommand, SessionSetCommand};
use gleaph_gql::parser;
use gleaph_graph_kernel::entry::GraphId;

use crate::facade::stable::graph_catalog;
use crate::facade::store::RouterStore;
use crate::state::RouterError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedGraphContext {
    pub graph_id: GraphId,
}

/// Resolve the effective graph for plain query execution from `program`.
pub fn resolve_graph_context(
    store: &RouterStore,
    program: &GqlProgram,
    caller: candid::Principal,
) -> Result<ResolvedGraphContext, RouterError> {
    let mut current: Option<GraphId> = None;
    for cmd in &program.session_activity {
        current = apply_session_command(store, cmd, caller, current)?;
    }
    let graph_id = match current {
        Some(id) => id,
        None => resolve_default_graph(store, caller)?,
    };
    Ok(ResolvedGraphContext { graph_id })
}

/// Parse and resolve graph context from a query string.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "used by integration tests and future ingress")
)]
pub fn resolve_graph_context_from_query(
    store: &RouterStore,
    query: &str,
    caller: candid::Principal,
) -> Result<ResolvedGraphContext, RouterError> {
    let program = parser::parse(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    resolve_graph_context(store, &program, caller)
}

fn apply_session_command(
    store: &RouterStore,
    cmd: &SessionCommand,
    caller: candid::Principal,
    current: Option<GraphId>,
) -> Result<Option<GraphId>, RouterError> {
    match cmd {
        SessionCommand::Set(set) => apply_session_set(store, set, caller, current),
        SessionCommand::Reset(reset) => apply_session_reset(reset, current),
        SessionCommand::Close => Ok(current),
    }
}

fn apply_session_set(
    store: &RouterStore,
    set: &SessionSetCommand,
    caller: candid::Principal,
    current: Option<GraphId>,
) -> Result<Option<GraphId>, RouterError> {
    match set {
        SessionSetCommand::Graph { name, .. } => {
            resolve_graph_reference(store, name, caller, current).map(Some)
        }
        SessionSetCommand::GraphParameter { .. } => Err(RouterError::InvalidArgument(
            "SESSION SET GRAPH parameter form is not supported on router ingress yet".into(),
        )),
        _ => Ok(current),
    }
}

fn apply_session_reset(
    reset: &SessionResetCommand,
    current: Option<GraphId>,
) -> Result<Option<GraphId>, RouterError> {
    match reset {
        SessionResetCommand::Graph { .. } | SessionResetCommand::All => Ok(None),
        _ => Ok(current),
    }
}

fn resolve_graph_reference(
    store: &RouterStore,
    name: &gleaph_gql::ast::ObjectName,
    caller: candid::Principal,
    current: Option<GraphId>,
) -> Result<GraphId, RouterError> {
    if name.parts.len() != 1 {
        return Err(RouterError::InvalidArgument(
            "catalog-qualified graph names are not supported yet".into(),
        ));
    }
    match name.parts[0].as_str() {
        "CURRENT_GRAPH" => current.ok_or_else(|| {
            RouterError::InvalidArgument("CURRENT_GRAPH is unset in this program".into())
        }),
        "HOME_GRAPH" => resolve_home_graph(store, caller),
        other => {
            store.resolve_graph(other, caller)?;
            graph_catalog::lookup_graph_id(other)
                .ok_or_else(|| RouterError::NotFound(other.to_owned()))
        }
    }
}

fn resolve_default_graph(
    store: &RouterStore,
    caller: candid::Principal,
) -> Result<GraphId, RouterError> {
    resolve_home_graph(store, caller)
}

/// Default graph for ingress without `SESSION SET GRAPH` (HOME = sole visible graph).
pub fn resolve_default_graph_id(
    store: &RouterStore,
    caller: candid::Principal,
) -> Result<GraphId, RouterError> {
    resolve_default_graph(store, caller)
}

fn resolve_home_graph(
    store: &RouterStore,
    caller: candid::Principal,
) -> Result<GraphId, RouterError> {
    let visible = store.list_visible_graph_ids(caller)?;
    match visible.as_slice() {
        [only] => Ok(*only),
        [] => Err(RouterError::InvalidArgument("no graph context".into())),
        _ => Err(RouterError::InvalidArgument(
            "HOME_GRAPH is ambiguous: multiple graphs visible to caller".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};

    fn register_graph(store: &RouterStore, name: &str) {
        let owner = Principal::anonymous();
        store.bootstrap_controllers(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: name.to_owned(),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                },
            )
            .expect("register");
    }

    #[test]
    fn sole_visible_graph_is_default_without_session_set() {
        let store = RouterStore::new();
        register_graph(&store, "gleaph.pocket_ic");
        let ctx =
            resolve_graph_context_from_query(&store, "MATCH (n) RETURN n", Principal::anonymous())
                .expect("resolve");
        assert_eq!(
            graph_catalog::lookup_graph_id("gleaph.pocket_ic"),
            Some(ctx.graph_id)
        );
    }

    #[test]
    fn session_set_graph_overrides_default() {
        let store = RouterStore::new();
        register_graph(&store, "tenant_a");
        register_graph(&store, "tenant_b");
        let caller = Principal::anonymous();
        let ctx = resolve_graph_context_from_query(
            &store,
            "SESSION SET GRAPH tenant_b MATCH (n) RETURN n",
            caller,
        )
        .expect("resolve");
        assert_eq!(
            graph_catalog::lookup_graph_id("tenant_b"),
            Some(ctx.graph_id)
        );
    }
}
