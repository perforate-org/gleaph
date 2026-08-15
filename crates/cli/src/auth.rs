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
