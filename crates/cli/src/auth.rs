//! `gleaph login` — resolve the caller's principal and store it as the active session.
//!
//! Delegates the browser/Internet Identity flow to `icp identity link web`, or reads a local
//! PEM identity's principal. The resolved principal is persisted so subsequent commands act as
//! that principal.

use ic_agent::Identity;
use std::path::Path;

/// Resolve the caller's principal from a local PEM identity (no browser flow).
///
/// `--identity` is a PEM path (current CLI convention). Returns the principal as text.
pub fn principal_from_pem(identity: &Path) -> Result<String, String> {
    let id = ic_agent::identity::Secp256k1Identity::from_pem_file(identity)
        .map_err(|e| format!("read identity {}: {e}", identity.display()))?;
    Ok(id
        .sender()
        .unwrap_or(candid::Principal::anonymous())
        .to_text())
}

/// The active session principal. Currently read from the explicit PEM identity; a web
/// (icp identity link web) delegation flow is a later slice.
pub fn resolve_principal(identity: Option<&Path>) -> Result<String, String> {
    match identity {
        Some(path) => principal_from_pem(path),
        None => {
            Err("no identity; pass --identity <PEM> or run `gleaph login` with an identity".into())
        }
    }
}

/// Run `icp identity link web` to obtain an Internet Identity delegation, then return the
/// principal for the linked identity.
///
/// The flow is interactive: `icp identity link web` prints a "Press Enter to log in" prompt,
/// opens (or prints) a sign-in URL, and blocks until the user completes the browser flow. The
/// principal for the resulting identity is read back with `icp identity principal --identity`.
pub fn login_with_web(name: &str, app: &str) -> Result<String, String> {
    // link web is interactive and long-running; it must run in the user's terminal, not
    // captured here. Delegate to icp-cli and return the principal afterward.
    let status = std::process::Command::new("icp")
        .args(["identity", "link", "web", name, "--app", app])
        .status()
        .map_err(|e| format!("run `icp identity link web`: {e}"))?;
    if !status.success() {
        return Err(format!(
            "`icp identity link web` failed with status {status}"
        ));
    }
    let output = std::process::Command::new("icp")
        .args(["identity", "principal", "--identity", name])
        .output()
        .map_err(|e| format!("run `icp identity principal`: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_owned())
}
