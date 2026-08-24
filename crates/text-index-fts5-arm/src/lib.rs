//! FTS5-on-VFS comparison arm for decision point D1 (plan 0293).
//!
//! **Boundary.** PoC comparison harness only, never production. This crate puts real
//! canbench instruction / stable-memory numbers for SQLite FTS5 (via `ic-sqlite-vfs`) next
//! to the custom posting kernels of `ic-stable-text-postings`. It depends on
//! [`ic_stable_text_postings`] **for the corpus fixture only** (single source of fixture
//! truth), and on SQLite via ic-sqlite-vfs as the alternative engine under test. It must
//! never contribute code, types, or semantics to any production text-index layer, and it
//! owns no GQL/Router/graph concepts.
//!
//! Module map: [`fixture`] holds the shared deterministic fixture (pure Rust, host-testable);
//! `bench` (wasm32 + `canbench` feature only) holds the measured SQLite workloads.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]
#![warn(missing_docs)]

pub mod fixture;

#[cfg(all(feature = "canbench", target_arch = "wasm32"))]
mod bench;
