//! Re-export of the shared IC ingress transport ([`gleaph_ingress_client`]).
//!
//! The transport layers ([`IcIngress`] — the generic any-canister/any-method caller — and
//! [`ProvisionClient`] — the typed Provision surface implementing
//! [`gleaph_artifact_api::ArtifactTransport`]) moved to the shared `gleaph-ingress-client`
//! crate so the dev CLI's `network start` catalog seeding can reuse them without pulling the
//! operator crate (ADR 0087: pure-CLI bring-up seeds the local catalog through the shared
//! ingestion library). The operator keeps this module so `commands.rs` / `bootstrap.rs` and
//! downstream users keep their import paths.
//!
//! Failure policies (transport-failure panic inside driver runs, typed preflight calls) are
//! documented on the shared crate.

pub use gleaph_ingress_client::{IngressError, IcIngress, ProvisionClient};