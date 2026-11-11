//! `gleaph text-index` — read one TEXT index definition and its canister counters.
//!
//! The counters distinguish the two failure modes of the text pipeline: documents never reaching
//! the canister (`ndocs == 0`, `pending_ops == 0`) versus delivered-but-not-flushed work
//! (`pending_ops > 0`) versus flushed documents whose postings still do not rank.

use std::path::PathBuf;

use candid::Principal;
use clap::Args;
use serde::Deserialize;

use crate::remote::RemoteTransport;
use gleaph_graph_kernel::federation::RouterError;

/// Arguments for `gleaph text-index`.
#[derive(Debug, Args)]
pub struct TextIndexArgs {
    /// Logical graph name holding the TEXT index.
    #[arg(long, value_name = "GRAPH")]
    pub graph: String,
    /// Graph-scoped TEXT index name (`CREATE TEXT INDEX <name>`).
    #[arg(long, value_name = "NAME")]
    pub name: String,
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Network name (`ic`/`local`) or endpoint.
    #[arg(long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM identity file.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long)]
    pub fetch_root_key: bool,
    /// Print the raw counters as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Candid mirror of `gleaph_router::types::TextIndexInfo` (field order is the wire contract).
#[derive(Debug, Clone, candid::CandidType, Deserialize)]
pub struct TextIndexInfo {
    /// Stable text-index id.
    pub text_index_id: u32,
    /// Indexed vertex label.
    pub label_id: u16,
    /// Indexed vertex property.
    pub property_id: u32,
    /// Pinned analyzer pipeline id.
    pub analyzer_id: u32,
    /// Provisioned text canister; `None` in dev mode.
    pub canister: Option<Principal>,
    /// Backfill status.
    pub status: TextIndexStatus,
}

/// Candid mirror of `gleaph_router::types::TextIndexStatusView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, candid::CandidType, Deserialize)]
pub enum TextIndexStatus {
    /// Declared without a provisioned canister.
    Registered,
    /// Provisioned; migration-driven backfill in flight.
    Backfilling,
    /// Backfill converged; planner/query visible.
    Ready,
}

/// Candid mirror of `text_canister::TextIndexStats` (field order is the wire contract).
#[derive(Debug, Clone, Copy, candid::CandidType, Deserialize)]
pub struct TextIndexStats {
    /// Pinned analyzer pipeline id.
    pub analyzer_id: u32,
    /// Live (non-tombstoned) documents.
    pub ndocs: u64,
    /// Live posted units across all terms.
    pub total_units: u64,
    /// Tombstoned docids awaiting reclaim.
    pub tombstoned_docs: u64,
    /// Ops still awaiting `admin_flush`.
    pub pending_ops: u64,
    /// Segment count (1 in v0).
    pub segments: u32,
    /// Next docid to allocate.
    pub next_docid: u32,
}

/// One TEXT index definition plus its canister counters.
#[derive(Debug, Clone)]
pub struct TextIndexReport {
    /// Definition row.
    pub info: TextIndexInfo,
    /// Counters when a canister is attached.
    pub stats: Option<TextIndexStats>,
    /// Router-sourced DML catalog entries for the graph (what the dispatcher gate receives).
    pub catalog: Vec<gleaph_graph_kernel::index::IndexedTextSpecEntry>,
}

/// Read the definition through Router and, when attached, the canister counters.
pub fn status(
    transport: &RemoteTransport,
    graph: &str,
    name: &str,
) -> Result<TextIndexReport, String> {
    let info = transport
        .query_args::<TextIndexInfo, RouterError>("get_text_index", (graph, name))
        .map_err(|error| format!("query get_text_index: {error}"))?
        .map_err(|error| format!("Router rejected get_text_index: {error:?}"))?;
    let stats = match info.canister {
        Some(canister) => Some(
            transport
                .query_plain::<TextIndexStats>(&canister, "get_stats", &())
                .map_err(|error| format!("query text canister get_stats: {error}"))?,
        ),
        None => None,
    };
    let catalog = transport
        .query_args::<Vec<gleaph_graph_kernel::index::IndexedTextSpecEntry>, RouterError>(
            "get_indexed_text_catalog",
            (graph,),
        )
        .map_err(|error| format!("query get_indexed_text_catalog: {error}"))?
        .map_err(|error| format!("Router rejected get_indexed_text_catalog: {error:?}"))?;
    Ok(TextIndexReport {
        info,
        stats,
        catalog,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_renders_definition_and_counters() {
        let report = TextIndexReport {
            info: TextIndexInfo {
                text_index_id: 0,
                label_id: 1,
                property_id: 2,
                analyzer_id: 1,
                canister: None,
                status: TextIndexStatus::Ready,
            },
            catalog: vec![gleaph_graph_kernel::index::IndexedTextSpecEntry {
                property_id: 2,
                label_id: 1,
            }],
            stats: Some(TextIndexStats {
                analyzer_id: 1,
                ndocs: 8,
                total_units: 40,
                tombstoned_docs: 0,
                pending_ops: 0,
                segments: 1,
                next_docid: 8,
            }),
        };
        let stats = report.stats.expect("stats");
        assert_eq!(stats.ndocs, 8);
        assert_eq!(report.info.status, TextIndexStatus::Ready);
        assert_eq!(report.catalog.len(), 1);
    }
}
