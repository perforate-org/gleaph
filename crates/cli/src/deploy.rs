//! `gleaph deploy` — provision the user's Router and graph (ADR 0068).
//!
//! Flow: register the caller's Account (if absent), authorize the first-Router issuance via
//! Provision, then resolve and cache the Router id.

use crate::auth;
use crate::config::{self, ConfigEnv, LoadedConfig};
use crate::remote::{RemoteTransport, resolve_router_id};
use candid::Principal;
use std::path::{Path, PathBuf};

/// Register the caller's Account, issue the first Router via Provision, and cache the Router id.
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
    let provision_canister = mapping.get("provision").ok_or(
        "no provision canister in .gleaph/data/mappings; the platform must be deployed first",
    )?;

    // Use the explicit identity, else resolve the session's signing source (PEM path or
    // icp-cli identity, depending on icp.yaml presence).
    let identity: Option<PathBuf> = match identity {
        Some(path) => Some(path.to_owned()),
        None => {
            let session = auth::load_session();
            let has_icp_yaml = loaded
                .path
                .parent()
                .is_some_and(crate::identity::has_icp_yaml);
            session
                .as_ref()
                .map(|s| crate::identity::session_pem(s, has_icp_yaml))
                .transpose()?
        }
    };

    let transport = RemoteTransport::connect(
        account_canister,
        network,
        identity.as_deref(),
        fetch_root_key,
    )?;
    let account_principal = Principal::from_text(account_canister)
        .map_err(|e| format!("invalid account canister id: {e}"))?;
    let provision_principal = Principal::from_text(provision_canister)
        .map_err(|e| format!("invalid provision canister id: {e}"))?;

    // The caller must already have a registered account; deploy does not self-register.
    let accounts: Vec<String> = transport
        .query_plain(&account_principal, "resolve_my_accounts", &())
        .map_err(|e| format!("resolve_my_accounts: {e}"))?;
    let account_id = match accounts.as_slice() {
        [] => {
            return Err(
                "no account registered for this identity; register an account first".into(),
            );
        }
        [single] => single.clone(),
        _ => {
            return Err(format!(
                "multiple accounts ({}) registered; pass --account to disambiguate",
                accounts.len()
            ));
        }
    };

    // Authorize the first-Router issuance via Provision (Account is the bootstrap trust subject).
    let result: Result<gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse, String> =
        transport
            .update_on(
                &account_principal,
                "authorize_router_issuance",
                &(
                    account_id.clone(),
                    "default".to_owned(),
                    provision_principal,
                ),
            )
            .map_err(|e| format!("authorize_router_issuance: {e}"))?;
    match result {
        Ok(_) => println!("router issuance authorized"),
        Err(e) => return Err(format!("authorize_router_issuance: {e}")),
    }

    // Resolve the Router id and cache it.
    let router = resolve_router_id(&transport, &account_principal, "default")
        .map_err(|e| format!("resolve router: {e}"))?;
    config::write_router_cache(loaded, &environment, &router.to_text());
    println!("router resolved: {}", router);
    Ok(())
}
