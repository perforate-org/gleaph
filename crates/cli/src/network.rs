//! `gleaph network` — start a local IC network and deploy the platform canisters.
//!
//! When an `icp.yaml` is present, the network is started by delegating to `icp-cli` (so the same
//! network is used). The platform canisters (Account, Provision) are then deployed by calling the
//! management canister directly via `ic-agent`. Without an `icp.yaml`, a Gleaph-owned local
//! network is a later slice; for now this errors.

use crate::config::{self, LoadedConfig};
use crate::remote::RemoteTransport;
use ic_management_canister_types::{
    CanisterIdRecord, CanisterInstallMode, CreateCanisterArgs, InstallCodeArgs,
};
use std::collections::BTreeMap;
use std::path::Path;

/// Start the local network and deploy the platform canisters, writing the mapping.
///
/// `network` is the network name (default "local"). `project_root` is where `icp.yaml` is looked
/// up. `account_wasm` / `provision_wasm` are the local wasm paths (online distribution is a later
/// slice). Returns the platform mapping written.
pub fn start(
    network: &str,
    project_root: &Path,
    loaded: &LoadedConfig,
    account_wasm: &Path,
    provision_wasm: &Path,
) -> Result<BTreeMap<String, String>, String> {
    if !crate::identity::has_icp_yaml(project_root) {
        return Err(
            "no icp.yaml; a Gleaph-owned local network is not implemented yet. Add an icp.yaml \
             or run `gleaph network start` in an icp-cli project"
                .into(),
        );
    }

    // Delegate network start to icp-cli.
    let status = std::process::Command::new("icp")
        .args(["network", "start", network, "-d"])
        .status()
        .map_err(|e| format!("run `icp network start`: {e}"))?;
    if !status.success() {
        return Err(format!(
            "`icp network start {network}` failed with status {status}"
        ));
    }

    // Connect to the management canister (aaaaa-aa) on the local network.
    let transport = RemoteTransport::connect("aaaaa-aa", network, None, true)?;

    let account_id = deploy_canister(&transport, account_wasm)?;
    let provision_id = deploy_canister(&transport, provision_wasm)?;

    let mut mapping = BTreeMap::new();
    mapping.insert("account".to_owned(), account_id.to_text());
    mapping.insert("provision".to_owned(), provision_id.to_text());
    config::write_mapping(loaded, network, &mapping).map_err(|e| format!("write mapping: {e}"))?;
    Ok(mapping)
}

/// Create a canister and install the given wasm, returning its id.
fn deploy_canister(
    transport: &RemoteTransport,
    wasm_path: &Path,
) -> Result<candid::Principal, String> {
    let wasm =
        std::fs::read(wasm_path).map_err(|e| format!("read wasm {}: {e}", wasm_path.display()))?;

    let create_args = CreateCanisterArgs {
        settings: None,
        sender_canister_version: None,
    };
    let created: CanisterIdRecord = transport.management_call("create_canister", &create_args)?;
    let canister_id = created.canister_id;

    let install_args = InstallCodeArgs {
        mode: CanisterInstallMode::Install,
        canister_id,
        wasm_module: wasm,
        arg: Vec::new(),
        sender_canister_version: None,
    };
    transport.management_call::<()>("install_code", &install_args)?;

    Ok(canister_id)
}
