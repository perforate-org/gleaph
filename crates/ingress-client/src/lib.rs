//! Shared IC ingress transport and typed Provision client.
//!
//! Two layers with distinct owners:
//!
//! 1. [`ingress::IcIngress`] — the generic "any destination canister + any method" caller. It
//!    owns the ic-agent setup (network resolution, PEM identity, root-key fetch) and exposes
//!    raw and candid-typed update/query calls. This is the seam the bootstrap-tier
//!    management-canister commands require: they reuse this layer unchanged.
//! 2. [`client::ProvisionClient`] — the typed Provision surface. Its inherent methods return
//!    `Result<Result<T, E>, IngressError>` so callers can distinguish server rejections from
//!    transport failures, and its [`gleaph_artifact_api::ArtifactTransport`] implementation
//!    feeds the shared ingestion driver (`crates/artifact-api/src/driver.rs`) so protocol
//!    logic is never duplicated.
//!
//! # Why a separate crate
//!
//! [`gleaph-artifact-api`](gleaph_artifact_api) is the neutral wire-contract crate: by its own
//! documented convention it depends only on `candid`, `serde`, and `sha2` and must never
//! depend on an IC runtime. This crate holds everything that legitimately needs `ic-agent`,
//! `reqwest`, and the network-resolution conventions, so the neutrality contract survives
//! while the transport stops being private to one consumer. Consumers today:
//! `gleaph-operator` (re-exported unchanged) and `gleaph network start`'s catalog seeding —
//! pure-CLI bring-up seeds the local catalog through the shared ingestion library. A future online wasm-distribution path (replica fetch + local cache,
//! the launcher download pattern) replaces the directory-based source without touching this
//! layer.
//!
//! Transport-failure policy of the trait implementation: the trait signature carries only the
//! server's typed error channel, so an ingress failure *during* a driver run cannot be
//! surfaced through it without fabricating server state. The implementation therefore fails
//! loudly ([`client::transport_failure`]); ingestion is idempotent by design, so re-running
//! the command after fixing connectivity resumes from the server-reported state. Every
//! command first performs one typed call as a preflight, which surfaces ordinary transport
//! failures (bad endpoint, wrong principal, missing identity) through the normal error
//! channel.
//!
//! Two clippy allowances are structural here, not incidental: the trait pins its futures to
//! `Send` explicitly (`impl Future<…> + Send`), so implementations cannot use the `async fn`
//! shorthand ([`clippy::manual_async_fn`]), and its Ok/Err pairs carry the unboxed
//! wire-mirror error types by contract, matching the operator's own
//! `#[allow(clippy::result_large_err)]` precedent.

#![warn(missing_docs)]
#![allow(clippy::manual_async_fn, clippy::result_large_err)]

pub mod client;
pub mod ingress;
pub mod net;
pub mod wire;

pub use client::ProvisionClient;
pub use ingress::{IngressError, IcIngress};