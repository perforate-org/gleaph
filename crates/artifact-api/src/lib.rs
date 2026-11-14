//! Provision artifact-catalog wire contract and ingestion pipeline (ADR 0087 slice 2).
//!
//! This crate owns the artifact/release/install/audit wire types and the client-side ingestion
//! logic: chunk splitting, SHA-256/chunk-hash computation, publish → upload → status ordering,
//! and idempotent resume, with transport behind a trait ([`transport::ArtifactTransport`]) so the
//! logic is unit-testable off-IC.
//!
//! The Provision canister and `gleaph-ingress-client` re-export the types in [`types`], so the
//! server, operator and clients share one Rust definition. The Candid surface remains
//! `crates/provision/provision.did`, checked against the generated service by Provision's candid
//! test. The optional `stable` feature adds the Candid `Storable` impls Provision's stable maps
//! need, keeping `ic-stable-structures` out of client builds.
//!
//! Like the rest of the neutral `gleaph-*-api` family, the base crate depends only on `candid`,
//! `serde`, and `sha2`; it never depends on `gleaph-provision` or an IC runtime.

#![warn(missing_docs)]

#[cfg(feature = "stable")]
mod stable;

pub mod driver;
pub mod pipeline;
pub mod transport;
pub mod types;

pub use driver::{IngestError, IngestOutcome, ingest_artifact};
pub use pipeline::{ArtifactPlan, plan_artifact};
pub use transport::ArtifactTransport;
