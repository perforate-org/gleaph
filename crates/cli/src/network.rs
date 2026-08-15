//! `gleaph network` — start a local IC network and deploy the platform canisters.
//!
//! When an `icp.yaml` is present, the network is started and the platform canisters are deployed
//! by delegating to `icp-cli` (so the same network is used). Without an `icp.yaml`, a Gleaph-owned
//! local network is a later slice; for now this errors.

use crate::config::{self, LoadedConfig};
use std::collections::BTreeMap;
use std::path::Path;

/// Start the local network and write the platform mapping.
///
/// `network` is the network name (default "local"). `project_root` is where `icp.yaml` is looked
/// up. Returns the platform mapping written.
pub fn start(
    network: &str,
    project_root: &Path,
    loaded: &LoadedConfig,
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

    // Deploy the platform canisters (Account, Provision) and read their ids.
    // ponytail: the platform canister names and deploy flow are not yet defined; this writes a
    // placeholder mapping so the command shape is testable. Real deployment is a later slice.
    let mut mapping = BTreeMap::new();
    mapping.insert("account".to_owned(), "aaaaa-aa".to_owned());
    mapping.insert("provision".to_owned(), "bbbbb-bb".to_owned());
    config::write_mapping(loaded, network, &mapping).map_err(|e| format!("write mapping: {e}"))?;
    Ok(mapping)
}
