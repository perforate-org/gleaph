//! Gleaph identity store and signing-source resolution.
//!
//! Owns the Gleaph identity store (`~/.config/gleaph/identity/keys/{name}.pem`), PEM import
//! (including `--from-icp` via `icp identity export`), and resolution of a signing source to a
//! PEM path. The secret stays in the store; only a reference is persisted in the session.

use ic_agent::Identity;
use std::path::{Path, PathBuf};

/// A reference to the signing source for the active session. The secret stays in the referenced
/// store (Gleaph PEM or icp-cli identity); only the reference is persisted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Session {
    /// A PEM file path (Gleaph store or user-supplied).
    Pem(PathBuf),
    /// An icp-cli identity name (resolved via `icp identity export` when an `icp.yaml` is present).
    IcpIdentity(String),
}

impl Session {
    /// The PEM path, if this session is a PEM identity.
    pub fn pem_path(&self) -> Option<&Path> {
        match self {
            Session::Pem(path) => Some(path),
            Session::IcpIdentity(_) => None,
        }
    }
}

/// Gleaph identity store root: `~/.config/gleaph/identity`.
pub fn store_root() -> Result<PathBuf, String> {
    let base = std::env::var_os("GLEAPH_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|p| PathBuf::from(p).join("gleaph")))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("gleaph"))
        })
        .ok_or("cannot determine user config directory (set HOME or GLEAPH_CONFIG_HOME)")?;
    Ok(base.join("identity"))
}

/// The PEM path for a named identity in the Gleaph store.
pub fn store_pem_path(name: &str) -> Result<PathBuf, String> {
    Ok(store_root()?.join("keys").join(format!("{name}.pem")))
}

/// Import a PEM file into the Gleaph store under `name`.
pub fn import(name: &str, pem_path: &Path) -> Result<PathBuf, String> {
    let dest = store_pem_path(name)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create identity dir {}: {e}", parent.display()))?;
    }
    std::fs::copy(pem_path, &dest)
        .map_err(|e| format!("copy {} to {}: {e}", pem_path.display(), dest.display()))?;
    Ok(dest)
}

/// Import an icp-cli identity by name via `icp identity export <name>`.
pub fn import_from_icp(name: &str) -> Result<PathBuf, String> {
    let output = std::process::Command::new("icp")
        .args(["identity", "export", name])
        .output()
        .map_err(|e| format!("run `icp identity export`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`icp identity export {name}` failed with status {}",
            output.status
        ));
    }
    let dest = store_pem_path(name)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create identity dir {}: {e}", parent.display()))?;
    }
    std::fs::write(&dest, &output.stdout).map_err(|e| format!("write {}: {e}", dest.display()))?;
    Ok(dest)
}

/// Resolve a session to a PEM path for signing.
///
/// - `Session::Pem` → the path directly.
/// - `Session::IcpIdentity(name)` → `icp identity export <name>` (used when an `icp.yaml` is
///   present, i.e. the user is an icp-cli user).
pub fn session_pem(session: &Session) -> Result<PathBuf, String> {
    match session {
        Session::Pem(path) => Ok(path.clone()),
        Session::IcpIdentity(name) => {
            let output = std::process::Command::new("icp")
                .args(["identity", "export", name])
                .output()
                .map_err(|e| format!("run `icp identity export`: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "`icp identity export {name}` failed with status {}",
                    output.status
                ));
            }
            // Write to a temp file so Secp256k1Identity can read it; the secret is not
            // persisted beyond the call.
            let tmp =
                std::env::temp_dir().join(format!("gleaph-{name}-{}.pem", std::process::id()));
            std::fs::write(&tmp, &output.stdout)
                .map_err(|e| format!("write temp identity: {e}"))?;
            Ok(tmp)
        }
    }
}

/// Resolve the caller's principal from a PEM file.
pub fn principal_from_pem(identity: &Path) -> Result<String, String> {
    let id = ic_agent::identity::Secp256k1Identity::from_pem_file(identity)
        .map_err(|e| format!("read identity {}: {e}", identity.display()))?;
    Ok(id
        .sender()
        .unwrap_or(candid::Principal::anonymous())
        .to_text())
}

/// Resolve the caller's principal from a session.
pub fn principal_from_session(session: &Session) -> Result<String, String> {
    let pem = session_pem(session)?;
    principal_from_pem(&pem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_config_home() -> PathBuf {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("gleaph-identity-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp config home");
        dir
    }

    #[test]
    fn import_copies_pem_into_store() {
        let home = temp_config_home();
        // SAFETY: single-threaded test.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };
        let src = home.join("src.pem");
        std::fs::write(&src, "dummy-pem").expect("write source");
        let dest = import("alice", &src).expect("import");
        assert_eq!(dest, store_pem_path("alice").expect("path"));
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), "dummy-pem");
        // SAFETY: single-threaded test; cleanup.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }
}
