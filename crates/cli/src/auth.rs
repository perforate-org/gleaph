//! `gleaph login` — resolve the caller's principal and store the active session.
//!
//! The session stores a **reference to the signing source** (a PEM path or an icp identity
//! name), never the secret itself. Identity storage and signing-source resolution live in
//! [`crate::identity`].

use crate::identity::{self, Session};
use std::path::{Path, PathBuf};

/// Session file under the user config dir: `~/.config/gleaph/session`.
fn session_path() -> Result<PathBuf, String> {
    let base = std::env::var_os("GLEAPH_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|p| PathBuf::from(p).join("gleaph")))
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("gleaph"))
        })
        .ok_or("cannot determine user config directory (set HOME or GLEAPH_CONFIG_HOME)")?;
    Ok(base.join("session"))
}

/// Persist the active session (a reference to the signing source).
pub fn save_session(session: &Session) -> Result<(), String> {
    let path = session_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create config dir {}: {e}", parent.display()))?;
    }
    let text = match session {
        Session::Pem(path) => format!("pem:{}", path.display()),
        Session::IcpIdentity(name) => format!("icp:{}", name),
    };
    std::fs::write(&path, text).map_err(|e| format!("write session {}: {e}", path.display()))
}

/// Read the active session, if any.
pub fn load_session() -> Option<Session> {
    let path = session_path().ok()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let text = text.trim();
    text.strip_prefix("pem:")
        .map(|rest| Session::Pem(PathBuf::from(rest)))
        .or_else(|| {
            text.strip_prefix("icp:")
                .map(|rest| Session::IcpIdentity(rest.to_owned()))
        })
}

/// The active session principal. Prefers an explicit PEM identity, then the saved session.
///
/// `project_root` is used to detect an `icp.yaml` (an icp-cli user), which selects how an
/// `IcpIdentity` session resolves its signing key.
pub fn resolve_principal(
    identity: Option<&Path>,
    project_root: Option<&Path>,
) -> Result<String, String> {
    match identity {
        Some(path) => identity::principal_from_pem(path),
        None => {
            let session: Session = load_session().ok_or_else(|| {
                "no identity; pass --identity <PEM>, run `gleaph login`, or set a session"
                    .to_owned()
            })?;
            let has_icp_yaml = project_root.is_some_and(identity::has_icp_yaml);
            identity::principal_from_session(&session, has_icp_yaml)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_config_home() -> PathBuf {
        let nonce = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("gleaph-auth-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp config home");
        dir
    }

    #[test]
    fn session_round_trips_via_config_home() {
        let home = temp_config_home();
        // SAFETY: single-threaded test; no other thread reads this env var concurrently.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };
        assert_eq!(load_session(), None);
        save_session(&Session::Pem(PathBuf::from("/tmp/id.pem"))).expect("save");
        assert_eq!(
            load_session(),
            Some(Session::Pem(PathBuf::from("/tmp/id.pem")))
        );
        save_session(&Session::IcpIdentity("demo-admin".into())).expect("save icp");
        assert_eq!(
            load_session(),
            Some(Session::IcpIdentity("demo-admin".into()))
        );
        // SAFETY: single-threaded test; cleanup after the assertion.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }
}
