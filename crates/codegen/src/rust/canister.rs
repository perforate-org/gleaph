//! Rust canister output profile for typed prepared operations.
//!
//! Generated code binds typed prepared operations directly to the `gleaph-cdk` client: a
//! `Prepared` marker type and a `PreparedExt` extension trait implemented for
//! `GleaphClient<Prepared>` wrap the Router's `prepared_query` / `prepared_mutate` endpoints.
//!
//! This profile is the Rust canister binding of [`super::shared`]; the shared renderer owns the
//! generated constructs and this module selects the `gleaph_cdk` runtime profile.

use super::shared::{RuntimeProfile, generate_rust_prepared};
use crate::{ManifestError, PreparedManifest};

/// The `gleaph_cdk` runtime profile: Candid row derives are emitted because canisters return
/// prepared rows over their Candid interface.
pub const CDK_PROFILE: RuntimeProfile = RuntimeProfile {
    path: "gleaph_cdk",
    candid_row_derive: true,
};

/// Generate Rust canister declarations and a `PreparedExt` operations trait.
pub fn generate_rust_canister(manifest: &PreparedManifest) -> Result<String, ManifestError> {
    generate_rust_prepared(manifest, CDK_PROFILE)
}
