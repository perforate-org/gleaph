//! `gleaph deploy` — provision the user's Router and graph (ADR 0068).
//!
//! Dev-mode flow (the implemented path): install the Router, graph-index, and graph-shard
//! canisters directly via the management canister, register the graph + shard through Router
//! `register_graph`, register the issued Router under the caller's Account, and cache the Router
//! id. This mirrors the platform half of `scripts/deploy-demo-local.sh` and the dev-mode Router
//! registration contract in `crates/router/src/api/control.rs`. The Provision artifact-catalog
//! issuance path (`LogicalResource::Router` via `accept_envelope`) remains proposed in ADR 0035 /
//! 0068 and is not driven here.

use crate::auth;
use crate::config::{self, ConfigEnv, LoadedConfig};
use crate::remote::RemoteTransport;
use candid::{CandidType, Encode, Principal};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::provisioning::init_args::{GraphInitArgs, IndexInitArgs, RouterInitArgs};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Dev-mode `Router.register_graph` argument wire mirror (the CLI does not depend on the Router
/// crate; Candid is structural). Matches `gleaph_router::types::RegisterGraphArgs`.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
struct RegisterGraphArgsWire {
    pub graph_name: String,
    pub owner: Principal,
    pub admins: BTreeSet<Principal>,
    pub is_home: bool,
    pub shards: Vec<RegisterGraphShardWire>,
    pub requested_resources: Vec<gleaph_graph_kernel::provisioning::wire::ProvisionableResource>,
}

/// One dev-mode shard placement wire mirror (`gleaph_router::types::RegisterGraphShard`).
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
struct RegisterGraphShardWire {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
}

/// `Account.register_router` argument wire mirror (`gleaph_account::types::RouterEntry`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
struct RouterEntryWire {
    pub router_id: String,
    pub router_canister: Principal,
}

/// Provision the caller's Router and graph in dev mode, then cache the Router id.
///
/// Requires an already-registered account (deploy does not self-register). Idempotent: when the
/// Router cache already resolves and `Account.resolve_router("default")` succeeds, this is a no-op
/// that reports the existing Router without re-creating any canister.
#[allow(clippy::too_many_arguments)]
pub fn deploy(
    network: &str,
    identity: Option<&Path>,
    fetch_root_key: bool,
    env: &ConfigEnv,
    loaded: Option<&LoadedConfig>,
    router_wasm: &Path,
    graph_index_wasm: &Path,
    graph_shard_wasm: &Path,
    graph: &str,
) -> Result<(), String> {
    let loaded = loaded.ok_or("no gleaph.toml; `gleaph deploy` needs a project config")?;
    let environment = config::effective_environment(env, network);
    let mapping =
        config::read_mapping(loaded, &environment).map_err(|e| format!("read mapping: {e}"))?;
    let account_canister = mapping.get("account").ok_or(
        "no account canister in .gleaph/data/mappings; the platform must be deployed first",
    )?;

    // Validate all wasm paths before any remote call so a missing artifact fails fast with no
    // canister side effect.
    for (label, path) in [
        ("--router-wasm", router_wasm),
        ("--graph-index-wasm", graph_index_wasm),
        ("--graph-shard-wasm", graph_shard_wasm),
    ] {
        if !path.is_file() {
            return Err(format!("{label}: wasm not found at {}", path.display()));
        }
    }
    if graph.is_empty() {
        return Err("--graph must be a non-empty logical graph name".into());
    }

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

    // Idempotency guard: if the Router is already issued and resolvable, report it and stop.
    if let Some(cached) = config::read_router_cache(loaded, &environment) {
        if router_resolves(&transport, &account_principal, &account_id, "default")? {
            println!("router already provisioned: {cached}");
            return Ok(());
        }
        // Cache is stale (Router missing from Account); fall through to re-provision.
        println!("router cache stale ({}); re-provisioning", cached);
    }

    let project_root = loaded.path.parent();
    let caller_text = auth::resolve_principal(identity.as_deref(), project_root)
        .map_err(|e| format!("resolve principal: {e}"))?;
    let caller =
        Principal::from_text(&caller_text).map_err(|e| format!("invalid caller principal: {e}"))?;
    let router_id = install_router(&transport, router_wasm, caller)?;
    println!("installed Router: {router_id}");

    let index_id = install_graph_index(&transport, graph_index_wasm, router_id)?;
    println!("installed graph-index: {index_id}");

    let graph_id = install_graph_shard(&transport, graph_shard_wasm, router_id, index_id, graph)?;
    println!("installed graph-shard: {graph_id}");

    register_graph(&transport, router_id, graph, caller, graph_id, index_id)?;
    println!("registered graph `{graph}` in Router {router_id}");

    register_router(&transport, &account_principal, &account_id, router_id)?;
    println!("registered Router {router_id} under account {account_id}");

    config::write_router_cache(loaded, &environment, &router_id.to_text());
    println!("router resolved: {router_id}");
    Ok(())
}

/// Check whether `Account.resolve_router("default")` returns a Router for the account.
fn router_resolves(
    transport: &RemoteTransport,
    account_canister: &Principal,
    account_id: &str,
    router_id: &str,
) -> Result<bool, String> {
    let result: Result<Principal, String> = transport
        .query_on(account_canister, "resolve_router", &(account_id, router_id))
        .map_err(|e| format!("resolve_router: {e}"))?;
    match result {
        Ok(_) => Ok(true),
        Err(e) => Err(format!("resolve_router: {e}")),
    }
}

/// Create + install the Router canister with dev-mode init args (`provision_canister: None`).
fn install_router(
    transport: &RemoteTransport,
    wasm_path: &Path,
    caller: Principal,
) -> Result<Principal, String> {
    let init = Encode!(&RouterInitArgs {
        issuing_principal: caller,
        initial_admins: Vec::new(),
        provision_canister: None,
    })
    .map_err(|e| format!("encode RouterInitArgs: {e}"))?;
    crate::network::install_canister(transport, wasm_path, init)
}

/// Create + install the graph-index canister, trusting the Router as its caller.
fn install_graph_index(
    transport: &RemoteTransport,
    wasm_path: &Path,
    router_id: Principal,
) -> Result<Principal, String> {
    let init = Encode!(&IndexInitArgs {
        router_canister: router_id,
    })
    .map_err(|e| format!("encode IndexInitArgs: {e}"))?;
    crate::network::install_canister(transport, wasm_path, init)
}

/// Create + install the graph-shard canister with federation routing to the Router and index.
fn install_graph_shard(
    transport: &RemoteTransport,
    wasm_path: &Path,
    router_id: Principal,
    index_id: Principal,
    graph: &str,
) -> Result<Principal, String> {
    let init = Encode!(&GraphInitArgs {
        logical_graph_name: Some(graph.to_owned()),
        router_canister: Some(router_id),
        shard_id: Some(ShardId::new(0)),
        index_canister: Some(index_id),
    })
    .map_err(|e| format!("encode GraphInitArgs: {e}"))?;
    crate::network::install_canister(transport, wasm_path, init)
}

/// Call Router `register_graph` (dev mode) with the single caller-installed shard.
fn register_graph(
    transport: &RemoteTransport,
    router_id: Principal,
    graph: &str,
    caller: Principal,
    graph_canister: Principal,
    index_canister: Principal,
) -> Result<(), String> {
    let args = RegisterGraphArgsWire {
        graph_name: graph.to_owned(),
        owner: caller,
        admins: BTreeSet::new(),
        is_home: false,
        shards: vec![RegisterGraphShardWire {
            shard_id: ShardId::new(0),
            graph_canister,
            index_canister,
        }],
        requested_resources: Vec::new(),
    };
    let result: Result<(), RouterError> = transport
        .update_on(&router_id, "register_graph", &args)
        .map_err(|e| format!("register_graph: {e}"))?;
    result.map_err(|e| format!("register_graph: {e}"))
}

/// Register the issued Router under the caller's account so `Account.resolve_router` succeeds.
fn register_router(
    transport: &RemoteTransport,
    account_canister: &Principal,
    account_id: &str,
    router_id: Principal,
) -> Result<(), String> {
    let entry = RouterEntryWire {
        router_id: "default".to_owned(),
        router_canister: router_id,
    };
    let result: Result<(), String> = transport
        .update_on(account_canister, "register_router", &(account_id, entry))
        .map_err(|e| format!("register_router: {e}"))?;
    result.map_err(|e| format!("register_router: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Decode;

    #[test]
    fn router_entry_wire_is_candid_round_trippable() {
        let entry = RouterEntryWire {
            router_id: "default".to_owned(),
            router_canister: Principal::from_slice(&[7; 29]),
        };
        let bytes = Encode!(&entry).expect("encode");
        let decoded: RouterEntryWire = Decode!(&bytes, RouterEntryWire).expect("decode");
        assert_eq!(decoded, entry);
    }

    #[test]
    fn register_graph_wire_matches_router_schema() {
        let args = RegisterGraphArgsWire {
            graph_name: "social".to_owned(),
            owner: Principal::from_slice(&[1; 29]),
            admins: BTreeSet::new(),
            is_home: false,
            shards: vec![RegisterGraphShardWire {
                shard_id: ShardId::new(0),
                graph_canister: Principal::from_slice(&[2; 29]),
                index_canister: Principal::from_slice(&[3; 29]),
            }],
            requested_resources: Vec::new(),
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded = Decode!(&bytes, RegisterGraphArgsWire).expect("decode");
        assert_eq!(decoded.graph_name, "social");
        assert_eq!(decoded.shards.len(), 1);
        assert_eq!(decoded.shards[0].shard_id, ShardId::new(0));
    }

    #[test]
    fn deploy_validates_wasm_paths_before_remote_calls() {
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("gleaph-deploy-wasm-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("temp root");
        let config_path = root.join("gleaph.toml");
        fs::write(
            &config_path,
            "format_version = 1\ndefault_network = \"local\"\n",
        )
        .expect("config write");

        let loaded = crate::config::Config::load(
            std::path::Path::new("."),
            &crate::config::ConfigEnv {
                config: Some(config_path.to_string_lossy().into_owned()),
                ..crate::config::ConfigEnv::default()
            },
        )
        .expect("config load")
        .expect("config exists");
        let mut mapping = std::collections::BTreeMap::new();
        mapping.insert("account".to_owned(), "aaaaa-aa".to_owned());
        mapping.insert("provision".to_owned(), "aaaaa-aa".to_owned());
        crate::config::write_mapping(&loaded, "local", &mapping).expect("write mapping");

        // A missing wasm path must fail fast with an actionable message before any network call.
        let err = deploy(
            "local",
            None,
            true,
            &crate::config::ConfigEnv::default(),
            Some(&loaded),
            std::path::Path::new("/nonexistent/router.wasm"),
            std::path::Path::new("/nonexistent/index.wasm"),
            std::path::Path::new("/nonexistent/graph.wasm"),
            "social",
        )
        .expect_err("missing router wasm must fail");
        assert!(
            err.contains("--router-wasm"),
            "missing wasm error must name the flag: {err}"
        );

        fs::remove_dir_all(root).expect("temp root cleanup");
    }
}
