//! Client-side mirror of the Provision artifact-catalog ingestion contract (ADR 0087 slice 2).
//!
//! This crate is the single source of truth for client-side ingestion protocol logic: chunk
//! splitting, SHA-256/chunk-hash computation, publish → upload → status ordering, and idempotent
//! resume, with transport behind a trait ([`transport::ArtifactTransport`]) so the logic is
//! unit-testable off-IC.
//!
//! The wire types in [`types`] mirror `crates/provision/provision.did` and
//! `crates/provision/src/types.rs`. That did and those Rust types are the authority; every
//! mirrored type cites its source file and line range, and declares fields and variants in
//! `provision.did` declaration order. Candid encodes records and variants by field-name hash,
//! so name equality is what guarantees wire compatibility — the matching order is kept so a
//! reviewer can diff this file against the did line by line.
//!
//! Like the rest of the neutral `gleaph-*-api` family, this crate depends only on `candid`,
//! `serde`, and `sha2`. It must never depend on `gleaph-provision` or any IC runtime;
//! drift from the server contract is caught by review against the did and by PocketIC E2E runs.

#![warn(missing_docs)]

pub mod driver;
pub mod pipeline;
pub mod transport;
pub mod types;

pub use driver::{IngestError, IngestOutcome, ingest_artifact};
pub use pipeline::{ArtifactPlan, plan_artifact};
pub use transport::ArtifactTransport;
