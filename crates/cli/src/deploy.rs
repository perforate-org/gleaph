//! `gleaph deploy` — provision the user's Router and graph (ADR 0068).
//!
//! Slice 1 scope: register the caller's Account (if absent) and write the platform-fixed
//! `.gleaph/data/mappings/<env>.ids.json`. Router issuance (Provision) is a later slice.

use crate::config::{self, ConfigEnv, LoadedConfig};
use crate::remote::{RemoteTransport, resolve_router_id};
use candid::Principal;
use std::path::Path;

/// Register the caller's Account and write the platform mapping.
///
/// Returns the Account canister id and the caller's account id.
pub fn deploy(
    network: &str,
    identity: Option<&Path>,
    fetch_root_key: bool,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
) -> Result<(), String> {
    let loaded = loaded.ok_or("no gleaph.toml; `gleaph deploy` needs a project config")?;
    let environment = config::effective_environment(env, network);
    let mapping =
        config::read_mapping(loaded, &environment).map_err(|e| format!("read mapping: {e}"))?;
    let account_canister = mapping.get("account").ok_or(
        "no account canister in .gleaph/data/mappings; the platform must be deployed first",
    )?;

    let transport = RemoteTransport::connect(account_canister, network, identity, fetch_root_key)?;
    let account_principal = Principal::from_text(account_canister)
        .map_err(|e| format!("invalid account canister id: {e}"))?;

    // Register the caller's Personal account if they have none.
    let accounts: Vec<String> = transport
        .query_plain(&account_principal, "resolve_my_accounts", &())
        .map_err(|e| format!("resolve_my_accounts: {e}"))?;
    if accounts.is_empty() {
        let name = "default";
        let result: Result<(), String> = transport
            .update_on(&account_principal, "create_account", &(name.to_owned()))
            .map_err(|e| format!("create_account: {e}"))?;
        result.map_err(|e| format!("create_account: {e}"))?;
        println!("registered account for this identity");
    } else {
        println!("account already registered");
    }

    // Resolve the Router id (may be unissued -> error for now; issuance is a later slice).
    let router = resolve_router_id(&transport, &account_principal, "default")
        .map_err(|e| format!("resolve router: {e}"))?;
    config::write_router_cache(loaded, &environment, &router.to_text());
    println!("router resolved: {}", router);
    Ok(())
}
