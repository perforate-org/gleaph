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
    validate_store_name(name)?;
    let dest = store_pem_path(name)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create identity dir {}: {e}", parent.display()))?;
    }
    std::fs::copy(pem_path, &dest)
        .map_err(|e| format!("copy {} to {}: {e}", pem_path.display(), dest.display()))?;
    Ok(dest)
}

/// A freshly generated identity: where its PEM lives and which principal it signs for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedIdentity {
    /// The PEM path written into the store (`keys/<name>.pem`).
    pub pem_path: PathBuf,
    /// The self-authenticating principal derived from the new key.
    pub principal: String,
}

/// Generate a fresh Secp256k1 signing key into the store under `name`.
///
/// Fails closed when the name is not a bare file stem or when `keys/<name>.pem` already
/// exists (the write uses `create_new`, so an existing identity is never overwritten). The
/// PEM is created with owner-only permissions on Unix. The principal is derived by reading
/// the stored PEM back through the same path [`principal_from_pem`] uses for `login`, so
/// the printed principal and every later session resolution agree.
pub fn create(name: &str) -> Result<CreatedIdentity, String> {
    use k256::elliptic_curve::Generate;

    validate_store_name(name)?;
    let dest = store_pem_path(name)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create identity dir {}: {e}", parent.display()))?;
    }
    let secret_key = k256::SecretKey::generate_from_rng(&mut rand::rng());
    use k256::elliptic_curve::pkcs8::EncodePrivateKey as _;
    let pem = secret_key
        .to_pkcs8_pem(k256::elliptic_curve::pkcs8::LineEnding::LF)
        .map_err(|e| format!("encode private key PEM: {e}"))?;
    write_store_pem_new(&dest, pem.as_bytes())?;
    let principal = principal_from_pem(&dest)?;
    Ok(CreatedIdentity {
        pem_path: dest,
        principal,
    })
}

/// Write fresh secret bytes as a new store file. `create_new` makes the collision check and
/// the creation one atomic step; no existing identity can be truncated or overwritten.
fn write_store_pem_new(dest: &Path, bytes: &[u8]) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dest)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    format!(
                        "identity already exists at {}; pick another name or drop the file first",
                        dest.display()
                    )
                } else {
                    format!("create {}: {e}", dest.display())
                }
            })?;
        file.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", dest.display()))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        if dest.exists() {
            return Err(format!(
                "identity already exists at {}; pick another name or drop the file first",
                dest.display()
            ));
        }
        std::fs::write(dest, bytes).map_err(|e| format!("write {}: {e}", dest.display()))
    }
}

/// Validate an identity store key. The name is the canonical store filename (`<name>.pem`);
/// path syntax would escape or redirect the keys directory, so it is rejected before any
/// filesystem use.
fn validate_store_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return Err(format!(
            "invalid identity name {name:?}; expected a bare name without path separators"
        ));
    }
    Ok(())
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
/// - `Session::IcpIdentity(name)` → if `has_icp_yaml`, `icp identity export <name>` (the user is
///   an icp-cli user); otherwise the Gleaph store's PEM for `name`.
pub fn session_pem(session: &Session, has_icp_yaml: bool) -> Result<PathBuf, String> {
    match session {
        Session::Pem(path) => Ok(path.clone()),
        Session::IcpIdentity(name) => {
            if has_icp_yaml {
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
            } else {
                // No icp.yaml: use the Gleaph store's PEM for the named identity.
                store_pem_path(name)
            }
        }
    }
}

/// True when an `icp.yaml` exists in the project root (the user is an icp-cli user).
pub fn has_icp_yaml(project_root: &Path) -> bool {
    project_root.join("icp.yaml").is_file()
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
pub fn principal_from_session(session: &Session, has_icp_yaml: bool) -> Result<String, String> {
    let pem = session_pem(session, has_icp_yaml)?;
    principal_from_pem(&pem)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static CONFIG_HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Serialize every test that mutates `GLEAPH_CONFIG_HOME`. The env var is process-global:
    /// concurrent tests could otherwise resolve each other's temp store — or worse, the real
    /// user config directory — mid-test.
    pub(crate) fn lock_config_home() -> MutexGuard<'static, ()> {
        CONFIG_HOME_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
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
        let _guard = test_support::lock_config_home();
        let home = temp_config_home();
        // SAFETY: single-threaded relative to the config-home lock; cleanup restores the env.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };
        let src = home.join("src.pem");
        std::fs::write(&src, "dummy-pem").expect("write source");
        let dest = import("alice", &src).expect("import");
        assert_eq!(dest, store_pem_path("alice").expect("path"));
        assert_eq!(std::fs::read_to_string(&dest).expect("read"), "dummy-pem");
        // SAFETY: see set_var above; cleanup after the assertions.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }

    #[test]
    fn create_writes_store_layout_and_derives_principal_via_login_path() {
        let _guard = test_support::lock_config_home();
        let home = temp_config_home();
        // SAFETY: single-threaded relative to the config-home lock; cleanup restores the env.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };
        let created = create("dev").expect("create identity");

        let expected_path = store_pem_path("dev").expect("store path");
        assert_eq!(created.pem_path, expected_path);
        let pem = std::fs::read_to_string(&expected_path).expect("stored PEM");
        assert!(
            pem.starts_with("-----BEGIN PRIVATE KEY-----"),
            "the store must hold a PKCS#8 PEM, got: {pem}"
        );

        // The printed principal must match what a later session resolution (`login`) reads
        // from the same stored PEM — one derivation path, no second principal source.
        assert_eq!(
            principal_from_pem(&expected_path).expect("resolve"),
            created.principal
        );
        assert_ne!(
            created.principal,
            candid::Principal::anonymous().to_text(),
            "a generated key must not be anonymous"
        );
        // SAFETY: see set_var above; cleanup after the assertions.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }

    #[test]
    fn create_name_collision_fails_and_preserves_the_existing_identity() {
        let _guard = test_support::lock_config_home();
        let home = temp_config_home();
        // SAFETY: single-threaded relative to the config-home lock; cleanup restores the env.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };
        let first = create("dup").expect("first create");
        let stored = std::fs::read_to_string(&first.pem_path).expect("stored PEM");

        let error = create("dup").expect_err("second create must fail");
        assert!(
            error.contains("already exists"),
            "collision must name the conflict, got: {error}"
        );
        // The original identity survives byte-for-byte; a failed create writes nothing.
        assert_eq!(
            std::fs::read_to_string(&first.pem_path).expect("reread"),
            stored,
            "a collided create must not touch the existing key"
        );
        // SAFETY: see set_var above; cleanup after the assertions.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }

    #[test]
    fn create_rejects_names_that_escape_the_store() {
        let _guard = test_support::lock_config_home();
        let home = temp_config_home();
        // SAFETY: single-threaded relative to the config-home lock; cleanup restores the env.
        unsafe { std::env::set_var("GLEAPH_CONFIG_HOME", &home) };
        for name in ["", ".", "..", "a/b", "..\\escape", "nul\0byte"] {
            let error = create(name).expect_err("path-like names must be rejected");
            assert!(error.contains("invalid identity name"), "got: {error}");
        }
        // Rejection happens before any filesystem use: the keys dir must not even exist.
        let keys_dir = store_root().expect("root").join("keys");
        assert!(
            !keys_dir.exists(),
            "rejected names must not touch the store"
        );
        // SAFETY: see set_var above; cleanup after the assertions.
        unsafe { std::env::remove_var("GLEAPH_CONFIG_HOME") };
    }

    #[test]
    fn has_icp_yaml_detects_project_file() {
        let root = temp_config_home();
        assert!(!has_icp_yaml(&root));
        std::fs::write(root.join("icp.yaml"), "networks: []").expect("write icp.yaml");
        assert!(has_icp_yaml(&root));
    }
}
