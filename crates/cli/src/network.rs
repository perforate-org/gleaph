//! `gleaph network` — start a local IC network and deploy the platform canisters.
//!
//! When an `icp.yaml` is present, the network is started by delegating to `icp-cli` (so the same
//! network is used). Without one, Gleaph downloads and runs the `icp-cli-network-launcher`
//! (a PocketIC-based local subnet) as a child process. The platform canisters (Account,
//! Provision) are then deployed by calling the management canister directly via `ic-agent`.

use crate::config::{self, LoadedConfig};
use crate::remote::RemoteTransport;
use ic_agent::Identity;
use candid::Encode;
use ic_management_canister_types::{
    CanisterIdRecord, CanisterInstallMode, CreateCanisterArgs, InstallCodeArgs,
    ProvisionalCreateCanisterWithCyclesArgs,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Child;

/// Result of starting a network.
pub struct StartResult {
    /// The platform mapping written.
    pub mapping: BTreeMap<String, String>,
    /// The gateway port, for a Gleaph-owned network.
    pub gateway_port: Option<u16>,
    /// The launcher child process, for a Gleaph-owned network (caller must keep it alive).
    pub launcher_child: Option<Child>,
}

/// Start the local network and deploy the platform canisters, writing the mapping.
///
/// `network` is the network name (default "local"). `project_root` is where `icp.yaml` is looked
/// up. `account_wasm` / `provision_wasm` are the local wasm paths (online distribution is a later
/// slice).
pub fn start(
    network: &str,
    project_root: &Path,
    loaded: &LoadedConfig,
    account_wasm: &Path,
    provision_wasm: &Path,
    background: bool,
    identity_pem: Option<&Path>,
) -> Result<StartResult, String> {
    let mut launcher_child = None;
    let mut gateway_port = None;
    if crate::identity::has_icp_yaml(project_root) {
        // Delegate network start to icp-cli.
        let mut cmd = std::process::Command::new("icp");
        cmd.args(["network", "start", network]);
        if background {
            cmd.arg("-d");
        }
        let status = cmd
            .status()
            .map_err(|e| format!("run `icp network start`: {e}"))?;
        if !status.success() {
            return Err(format!(
                "`icp network start {network}` failed with status {status}"
            ));
        }
    } else {
        // Gleaph-owned local network: download and run the launcher.
        let launcher = download_launcher()?;
        let (child, port) = spawn_launcher(&launcher, network, background)?;
        launcher_child = Some(child);
        gateway_port = Some(port);
    }

    // Connect to the management canister (aaaaa-aa) on the local network. The session identity
    // signs the deploy calls; its principal is Provision's governance authority at init.
    let transport = RemoteTransport::connect("aaaaa-aa", network, identity_pem, true)?;

    let account_id = deploy_canister(&transport, account_wasm)?;
    let governance_principal = identity_pem
        .map(identity_principal)
        .transpose()?
        .ok_or("no active identity; run `gleaph login` or `gleaph identity new` first")?;
    let init_arg = provision_init_args(governance_principal)?;
    let provision_id = install_canister(&transport, provision_wasm, init_arg)?;

    let mut mapping = BTreeMap::new();
    mapping.insert("account".to_owned(), account_id.to_text());
    mapping.insert("provision".to_owned(), provision_id.to_text());
    config::write_mapping(loaded, network, &mapping).map_err(|e| format!("write mapping: {e}"))?;
    Ok(StartResult {
        mapping,
        gateway_port,
        launcher_child,
    })
}

/// Download the `icp-cli-network-launcher` binary from GitHub Releases, caching it under the
/// user config dir. Returns the path to the launcher binary.
fn download_launcher() -> Result<PathBuf, String> {
    let cache_dir = std::env::var_os("GLEAPH_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("gleaph"))
        })
        .ok_or("cannot determine user config directory")?
        .join("launcher");
    let launcher_path = cache_dir.join("icp-cli-network-launcher");
    if launcher_path.is_file() {
        return Ok(launcher_path);
    }

    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        other => return Err(format!("unsupported arch {other}")),
    };
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => return Err(format!("unsupported os {other}")),
    };

    // Resolve the latest version from the GitHub API.
    let client = reqwest::blocking::Client::new();
    let latest: serde_json::Value = client
        .get("https://api.github.com/repos/dfinity/icp-cli-network-launcher/releases/latest")
        .header("User-Agent", "gleaph")
        .send()
        .map_err(|e| format!("fetch latest launcher version: {e}"))?
        .error_for_status()
        .map_err(|e| format!("fetch latest launcher version: {e}"))?
        .json()
        .map_err(|e| format!("parse latest launcher version: {e}"))?;
    let version = latest["tag_name"]
        .as_str()
        .ok_or("no tag_name in latest launcher release")?;

    let url = format!(
        "https://github.com/dfinity/icp-cli-network-launcher/releases/download/{version}/icp-cli-network-launcher-{arch}-{os}-{version}.tar.gz"
    );
    let bytes = client
        .get(&url)
        .header("User-Agent", "gleaph")
        .send()
        .map_err(|e| format!("download launcher: {e}"))?
        .error_for_status()
        .map_err(|e| format!("download launcher: {e}"))?
        .bytes()
        .map_err(|e| format!("read launcher download: {e}"))?;

    // Extract the tar.gz into the cache dir.
    std::fs::create_dir_all(&cache_dir).map_err(|e| format!("create launcher cache dir: {e}"))?;
    let gz = flate2::read::GzDecoder::new(&bytes[..]);
    let mut archive = tar::Archive::new(gz);
    archive
        .unpack(&cache_dir)
        .map_err(|e| format!("extract launcher: {e}"))?;

    // Release v15+ archives nest the binaries under a versioned directory
    // (`<name>-<arch>-<os>-<version>/icp-cli-network-launcher`, plus the bundled
    // `pocket-ic`), while the cache contract (and `spawn_launcher`) expects a bare
    // `icp-cli-network-launcher` path. Stage whatever layout the archive produced into the
    // flat cache shape.
    normalize_extracted_launcher(&cache_dir, &launcher_path)?;

    Ok(launcher_path)
}

/// Locate the extracted `icp-cli-network-launcher` binary anywhere under `cache_dir` and copy
/// it (with its sibling `pocket-ic`, which the launcher resolves next to itself) to the bare
/// `launcher_path` cache layout. Idempotent: an already-flat archive is a no-op.
fn normalize_extracted_launcher(cache_dir: &Path, launcher_path: &Path) -> Result<(), String> {
    if launcher_path.is_file() {
        return Ok(());
    }
    let mut found_launcher = None;
    let mut stack = vec![cache_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("read launcher cache: {e}"))?;
        for entry in entries {
            let path = entry
                .map_err(|e| format!("read launcher cache: {e}"))?
                .path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|name| name.to_str())
                == Some("icp-cli-network-launcher")
                && path != launcher_path
            {
                found_launcher = Some(path);
            }
        }
    }
    let Some(found) = found_launcher else {
        return Err("launcher archive contained no icp-cli-network-launcher binary".into());
    };
    std::fs::copy(&found, launcher_path).map_err(|e| format!("stage launcher binary: {e}"))?;
    if let Some(parent) = found.parent() {
        let pocket_ic = parent.join("pocket-ic");
        if pocket_ic.is_file() {
            std::fs::copy(&pocket_ic, cache_dir.join("pocket-ic"))
                .map_err(|e| format!("stage pocket-ic: {e}"))?;
        }
    }
    Ok(())
}

/// Spawn the launcher as a child process, waiting for its status file. Returns the child and the
/// gateway port.
///
/// In background mode the child is detached (its stdio is redirected) and the PID is recorded so
/// `gleaph network stop` can terminate it; the returned `Child` is still owned by the caller but
/// the process outlives it.
fn spawn_launcher(
    launcher_path: &Path,
    network: &str,
    background: bool,
) -> Result<(Child, u16), String> {
    let state_dir = std::env::temp_dir().join(format!("gleaph-{network}-state"));
    let status_dir = std::env::temp_dir().join(format!("gleaph-{network}-status"));

    // Refuse to start a second network while a background launcher is already running; its
    // state dir is in use and a fresh start would clobber it.
    if let Some(pid) = read_alive_pid(network)? {
        return Err(format!(
            "network '{network}' is already running (launcher pid {pid}); run `gleaph network stop` first"
        ));
    }

    // Start from a fresh state dir. The launcher persists PocketIC state across restarts, and a
    // previous run killed mid-write leaves the subnet state incomplete, which makes the next
    // launch panic ("The state of subnet ... is incomplete"). The official icp-cli removes the
    // state dir before each start; do the same (GAP-2026-08-24-006).
    if state_dir.exists() {
        std::fs::remove_dir_all(&state_dir)
            .map_err(|e| format!("remove stale state dir {}: {e}", state_dir.display()))?;
    }
    std::fs::create_dir_all(&state_dir).map_err(|e| format!("create state dir: {e}"))?;
    std::fs::create_dir_all(&status_dir).map_err(|e| format!("create status dir: {e}"))?;

    let mut cmd = std::process::Command::new(launcher_path);
    cmd.args([
        "--interface-version",
        "1.1.0",
        "--state-dir",
        state_dir.to_str().unwrap(),
        "--bind",
        "127.0.0.1",
        "--gateway-port",
        "8000",
        "--status-dir",
        status_dir.to_str().unwrap(),
    ]);
    if background {
        // Detach: redirect stdio to log files and create a new session so the launcher (and its
        // PocketIC process chain) survives the CLI's exit and a terminal close. Without `setsid`
        // the launcher stays in the CLI's session and dies on SIGHUP when the controlling
        // terminal closes (GAP-2026-08-24-006(b)).
        //
        // stdout/stderr go to files (not `/dev/null`) so a launcher that dies during startup
        // leaves a diagnosable trace; the official icp-cli does the same.
        let stdout_log = state_dir.join("launcher.stdout.log");
        let stderr_log = state_dir.join("launcher.stderr.log");
        let stdout = std::fs::File::create(&stdout_log)
            .map_err(|e| format!("create launcher stdout log {}: {e}", stdout_log.display()))?;
        let stderr = std::fs::File::create(&stderr_log)
            .map_err(|e| format!("create launcher stderr log {}: {e}", stderr_log.display()))?;
        cmd.stdout(std::process::Stdio::from(stdout));
        cmd.stderr(std::process::Stdio::from(stderr));
        cmd.stdin(std::process::Stdio::null());
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn launcher: {e}"))?;

    // Wait for the status file to appear, detecting a premature launcher exit so the caller gets
    // the launcher's stderr instead of a bare timeout.
    let status_file = status_dir.join("status.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&status_file) {
            let status: LauncherStatus =
                serde_json::from_str(&text).map_err(|e| format!("parse launcher status: {e}"))?;
            if status.v == "1" {
                if background {
                    // Record the PID so `gleaph network stop` can terminate it.
                    let _ = std::fs::write(pid_file(network), child.id().to_string());
                }
                return Ok((child, status.gateway_port));
            }
        }
        // If the launcher exited before writing a ready status file, surface its stderr.
        if let Some(status) = child.try_wait().map_err(|e| format!("wait launcher: {e}"))? {
            let stderr_log = state_dir.join("launcher.stderr.log");
            let tail = std::fs::read_to_string(&stderr_log)
                .unwrap_or_else(|_| "<stderr log unavailable>".to_string());
            return Err(format!(
                "launcher exited prematurely with {status} before writing a ready status file; \
                 stderr tail:\n{tail}"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Err("launcher did not become ready within 30s".into())
}

/// The PID file for a background network, under the user config dir.
fn pid_file(network: &str) -> PathBuf {
    std::env::var_os("GLEAPH_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("gleaph"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
        .join(format!("network-{network}.pid"))
}

/// Read the PID file and return the PID if the process is alive. If the file is absent, returns
/// `Ok(None)`. If the process is dead, removes the stale PID file and returns `Ok(None)`.
fn read_alive_pid(network: &str) -> Result<Option<u32>, String> {
    let path = pid_file(network);
    let pid_text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return Ok(None),
    };
    let pid: u32 = pid_text
        .trim()
        .parse()
        .map_err(|e| format!("parse pid: {e}"))?;
    // Probe the process without sending a signal (signal 0).
    let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
    if !alive {
        // Stale PID file: the process is gone. Clean it up.
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(pid))
}

/// Stop a background network by reading its PID file and terminating the process.
pub fn stop(network: &str) -> Result<(), String> {
    let path = pid_file(network);
    let Some(pid) = read_alive_pid(network)? else {
        // No live process (absent or stale PID file already cleaned up).
        return Ok(());
    };
    // Send SIGINT (like icp-cli) so the launcher cleans up.
    unsafe {
        libc::kill(pid as i32, libc::SIGINT);
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Report the status of a background network: whether the launcher process is alive and, if so,
/// the gateway port.
pub fn status(network: &str) -> Result<NetworkStatus, String> {
    let Some(pid) = read_alive_pid(network)? else {
        return Ok(NetworkStatus::NotRunning);
    };
    // Read the gateway port from the launcher status file.
    let status_dir = std::env::temp_dir().join(format!("gleaph-{network}-status"));
    let status_file = status_dir.join("status.json");
    let port = std::fs::read_to_string(&status_file)
        .ok()
        .and_then(|text| serde_json::from_str::<LauncherStatus>(&text).ok())
        .map(|s| s.gateway_port);
    Ok(NetworkStatus::Running { pid, port })
}

/// The status of a background network.
pub enum NetworkStatus {
    Running { pid: u32, port: Option<u16> },
    NotRunning,
}

/// The launcher's status file format (mirrors icp-cli).
// ponytail: root_key is read but not yet consumed; RemoteTransport::connect fetches the root key.
#[derive(Deserialize)]
#[allow(dead_code)]
struct LauncherStatus {
    v: String,
    gateway_port: u16,
    root_key: String,
}

/// Create a canister and install the given wasm with an empty init argument, returning its id.
fn deploy_canister(
    transport: &RemoteTransport,
    wasm_path: &Path,
) -> Result<candid::Principal, String> {
    install_canister(transport, wasm_path, Vec::new())
}

/// Candid mirror of the Provision canister's `ProvisionInitArgs`
/// (`crates/provision/provision.did`). Field names must match the did exactly.
#[derive(candid::CandidType)]
struct ProvisionInitArgsMirror {
    governance_principal: candid::Principal,
}

/// Resolve the principal behind a PEM identity file.
fn identity_principal(pem: &Path) -> Result<candid::Principal, String> {
    let identity = ic_agent::identity::Secp256k1Identity::from_pem_file(pem)
        .map_err(|e| format!("read identity {}: {e}", pem.display()))?;
    Ok(identity
        .sender()
        .unwrap_or(candid::Principal::anonymous()))
}

/// Encode the Provision init argument: the CLI user's principal becomes the single governance
/// authority. Deployment grants are seeded afterwards (`gleaph-operator grant upsert`).
fn provision_init_args(governance_principal: candid::Principal) -> Result<Vec<u8>, String> {
    if governance_principal == candid::Principal::anonymous() {
        return Err(
            "no active identity; run `gleaph login` or `gleaph identity new` before \
             network start (Provision requires a non-anonymous governance_principal)"
                .to_owned(),
        );
    }
    candid::Encode!(&ProvisionInitArgsMirror {
        governance_principal,
    })
    .map_err(|e| format!("encode provision init args: {e}"))
}

/// Create a canister and install the given wasm with the given Candid-encoded init argument,
/// returning its id.
///
/// `deploy` uses this to install the Router / graph-index / graph-shard canisters with their
/// typed init args; the platform canisters (Account / Provision) install with an empty argument.
pub(crate) fn install_canister(
    transport: &RemoteTransport,
    wasm_path: &Path,
    init_arg: Vec<u8>,
) -> Result<candid::Principal, String> {
    let wasm =
        std::fs::read(wasm_path).map_err(|e| format!("read wasm {}: {e}", wasm_path.display()))?;

    let canister_id = if transport.is_mainnet() {
        let create_args = CreateCanisterArgs {
            settings: None,
            sender_canister_version: None,
        };
        let created: CanisterIdRecord =
            transport.management_call("create_canister", &create_args)?;
        created.canister_id
    } else {
        // Local/PocketIC endpoints cannot route an ingress-level `create_canister` (the real
        // IC derives the target subnet from the caller+nonce; there is no such derived-id path
        // here). The local equivalent is `provisional_create_canister_with_cycles`, whose
        // response certification requires the effective canister id to fall within the target
        // subnet's canister ranges, so we use the network's default effective canister id from
        // `/_/topology` (GAP-2026-08-24-006(a)).
        let effective = transport.default_effective_canister_id().ok_or_else(|| {
            "local network did not expose a default effective canister id (/_/topology); \
             cannot route the provisional create"
                .to_string()
        })?;
        let provisional_args = ProvisionalCreateCanisterWithCyclesArgs {
            amount: None,
            settings: None,
            specified_id: None,
            sender_canister_version: None,
        };
        let created: CanisterIdRecord = transport.management_call_with_effective_canister_id(
            "provisional_create_canister_with_cycles",
            &provisional_args,
            effective,
        )?;
        created.canister_id
    };

    let install_args = InstallCodeArgs {
        mode: CanisterInstallMode::Install,
        canister_id,
        wasm_module: wasm,
        arg: init_arg,
        sender_canister_version: None,
    };
    transport.management_call_with_effective_canister_id::<()>(
        "install_code",
        &install_args,
        canister_id,
    )?;

    Ok(canister_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_config_home() -> PathBuf {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("gleaph-network-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp config home");
        dir
    }

    #[test]
    fn read_alive_pid_cleans_up_stale_file() {
        let _guard = crate::identity::test_support::lock_config_home();
        let home = temp_config_home();
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };

        // Absent file -> None.
        assert_eq!(read_alive_pid("local").expect("read"), None);

        // Spawn a short-lived child, wait for it to exit, then its PID is stale.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        child.wait().expect("wait child");

        let path = pid_file("local");
        std::fs::write(&path, pid.to_string()).expect("write pid");
        assert_eq!(read_alive_pid("local").expect("read"), None);
        assert!(!path.exists(), "stale pid file must be removed");

        // SAFETY: single-threaded test; cleanup.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }

    /// Stage a fake extracted archive layout: v15+ nests the binaries under a versioned
    /// directory.
    fn write_nested_archive(cache_dir: &Path) {
        let versioned = cache_dir.join("icp-cli-network-launcher-arm64-darwin-v15.0.0");
        std::fs::create_dir_all(&versioned).expect("versioned dir");
        std::fs::write(
            versioned.join("icp-cli-network-launcher"),
            b"launcher-bytes",
        )
        .expect("write launcher");
        std::fs::write(versioned.join("pocket-ic"), b"pocket-ic-bytes").expect("write pocket-ic");
    }

    #[test]
    fn normalize_stages_nested_launcher_and_companion() {
        let cache_dir = temp_config_home().join("launcher");
        std::fs::create_dir_all(&cache_dir).expect("cache dir");
        write_nested_archive(&cache_dir);
        let launcher_path = cache_dir.join("icp-cli-network-launcher");

        normalize_extracted_launcher(&cache_dir, &launcher_path).expect("normalize");

        assert_eq!(
            std::fs::read(&launcher_path).expect("launcher staged"),
            b"launcher-bytes"
        );
        assert_eq!(
            std::fs::read(cache_dir.join("pocket-ic")).expect("pocket-ic staged"),
            b"pocket-ic-bytes"
        );
    }

    #[test]
    fn normalize_is_idempotent_when_flat_binary_already_cached() {
        let cache_dir = temp_config_home().join("launcher");
        std::fs::create_dir_all(&cache_dir).expect("cache dir");
        let launcher_path = cache_dir.join("icp-cli-network-launcher");
        std::fs::write(&launcher_path, b"already-staged").expect("pre-staged");

        normalize_extracted_launcher(&cache_dir, &launcher_path).expect("no-op");

        // The pre-staged binary is left untouched; nothing else is created.
        assert_eq!(
            std::fs::read(&launcher_path).expect("reread"),
            b"already-staged"
        );
        assert!(!cache_dir.join("pocket-ic").exists());
    }

    #[test]
    fn normalize_fails_closed_when_the_archive_has_no_launcher() {
        let cache_dir = temp_config_home().join("launcher");
        std::fs::create_dir_all(&cache_dir).expect("cache dir");

        let error =
            normalize_extracted_launcher(&cache_dir, &cache_dir.join("icp-cli-network-launcher"))
                .expect_err("absent binary must fail");

        assert!(error.contains("contained no icp-cli-network-launcher"));
    }
}
