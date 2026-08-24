//! `gleaph embed ingest` — push deterministic vertex embeddings into a registered vector index
//! through Router's existing admin batch API (ADR 0064 §6 ingestion path; the B案 decision
//! keeps the durable bulk-load wire free of embedding bytes).
//!
//! The command runs strictly after a Completed bulk-load job: it re-reads the load's vertices
//! NDJSON for the `source_id` order, pages the job's chunk receipts, and reconstructs the
//! `source_id -> encoded vertex id` mapping by consuming `len(allocated_vertex_ids)` rows per
//! receipt in `chunk_index` order — receipts are the only ground truth because Router commits
//! budget-fitting prefixes. Every input is validated and digested before any remote call.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};

use candid::{CandidType, Encode};
use clap::Args;
use gleaph_bulk_load_api::{BulkLoadChunkReceiptV1, BulkLoadPublicStateV1, BulkLoadStatusPage};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::vector_index::{
    VertexEmbeddingIngestionResult, VertexEmbeddingProjectionOutcome,
};
use gleaph_message_sizing::{FitError, SizeHint, SizingPolicy, adaptive_fitting_prefix};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::progress::ProgressLine;
use crate::remote::RemoteTransport;

/// Bounded status paging, mirroring `load`.
const STATUS_PAGE_SIZE: u32 = 64;

#[derive(Debug, Args)]
pub struct EmbedIngestArgs {
    /// NDJSON vertices file in the same row order used for the completed bulk load.
    #[arg(long, value_name = "FILE")]
    pub vertices: PathBuf,
    /// NDJSON embeddings rows: {"source_id": "...", "values": [<number>...]}.
    #[arg(long, value_name = "FILE")]
    pub embeddings: PathBuf,
    /// Router canister principal (required unless supplied by GLEAPH_CANISTER or `gleaph.toml`).
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Logical graph name holding the registered vector index.
    #[arg(long, value_name = "NAME")]
    pub graph: Option<String>,
    /// Registered embedding (property) name; defaults to `embedding` (the DDL-interned name).
    #[arg(long, value_name = "NAME", default_value = "embedding")]
    pub embedding_name: String,
    /// Durable bulk-load job key whose receipts define the id mapping.
    #[arg(short = 'k', long, value_name = "KEY")]
    pub key: Option<String>,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, value_name = "NETWORK")]
    pub network: Option<String>,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    pub identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub fetch_root_key: Option<bool>,
    /// Load state file recording the effective job key (`[load] state_file`).
    #[arg(long, value_name = "PATH")]
    pub state_file: Option<PathBuf>,
}

/// Failures raised by `gleaph embed ingest`, mapped to the documented exit codes:
/// 1 operator action, 2 input validation, 3 remote/auth.
#[derive(Debug, Error)]
pub enum EmbedError {
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Artifact(String),
    #[error("{0}")]
    Remote(String),
    #[error("{0}")]
    Operator(String),
}

impl EmbedError {
    pub fn exit_code(&self) -> u8 {
        match self {
            EmbedError::Usage(_) | EmbedError::Artifact(_) => 2,
            EmbedError::Remote(_) => 3,
            EmbedError::Operator(_) => 1,
        }
    }
}

// ──── input model ────

/// One validated NDJSON embedding row.
#[derive(Clone, Debug)]
pub struct EmbeddingRow {
    pub source_id: String,
    pub values: Vec<f32>,
}

/// Candid wire mirror of the Router's `AdminIngestVertexEmbeddingBatchArgs` record
/// (`crates/router/src/types.rs`): field names and order are the wire contract. Kept local so
/// the CLI client does not depend on the Router server crate; the PocketIC flow test proves the
/// shape against the live endpoint.
#[derive(CandidType, Serialize, Clone, Debug)]
pub struct AdminIngestVertexEmbeddingBatchArgs {
    pub logical_graph_name: String,
    pub embedding_name: String,
    pub items: Vec<AdminIngestVertexEmbeddingBatchItem>,
}

#[derive(CandidType, Serialize, Clone, Debug)]
pub struct AdminIngestVertexEmbeddingBatchItem {
    pub encoded_vertex_id: Vec<u8>,
    pub values: Vec<f32>,
}

// ──── transport ────

/// Router endpoints consumed by one ingest run. The outer `Result` is a transport failure; the
/// inner `Result` is the decoded Router envelope.
pub trait EmbedIngestTransport {
    fn status(
        &mut self,
        graph: Option<&str>,
        key: &str,
        cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<Result<BulkLoadStatusPage, RouterError>, String>;

    fn ingest_batch(
        &mut self,
        args: &AdminIngestVertexEmbeddingBatchArgs,
    ) -> Result<Result<Vec<Result<VertexEmbeddingIngestionResult, String>>, RouterError>, String>;
}

struct RouterEmbedTransport {
    remote: RemoteTransport,
}

impl RouterEmbedTransport {
    fn connect(
        canister: &str,
        network: &str,
        identity: Option<&Path>,
        fetch_root_key: bool,
    ) -> Result<Self, EmbedError> {
        let remote = RemoteTransport::connect(canister, network, identity, fetch_root_key)
            .map_err(EmbedError::Remote)?;
        Ok(Self { remote })
    }
}

impl EmbedIngestTransport for RouterEmbedTransport {
    fn status(
        &mut self,
        graph: Option<&str>,
        key: &str,
        cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<Result<BulkLoadStatusPage, RouterError>, String> {
        // Multi-argument method: a tuple passed as one value would encode as a single record.
        self.remote.query_args(
            "bulk_load_status",
            (
                &graph.map(str::to_owned),
                &key.to_owned(),
                &cursor,
                &max_receipts,
            ),
        )
    }

    fn ingest_batch(
        &mut self,
        args: &AdminIngestVertexEmbeddingBatchArgs,
    ) -> Result<Result<Vec<Result<VertexEmbeddingIngestionResult, String>>, RouterError>, String>
    {
        self.remote.update("ingest_vertex_embeddings", args)
    }
}

// ──── input reading and validation ────

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Read every NDJSON embedding row, validate structural rules, and digest the raw bytes.
/// No remote call may happen before this succeeds.
fn scan_embeddings(path: &Path) -> Result<(Vec<EmbeddingRow>, String), EmbedError> {
    let file = fs::File::open(path)
        .map_err(|error| EmbedError::Artifact(format!("read {path:?}: {error}")))?;
    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(file);
    let mut rows: Vec<EmbeddingRow> = Vec::new();
    let mut source_ids: HashSet<String> = HashSet::new();
    let mut dimension: Option<usize> = None;
    let mut raw = String::new();
    let mut line_index = 0usize;
    loop {
        raw.clear();
        let n = reader
            .read_line(&mut raw)
            .map_err(|error| EmbedError::Artifact(format!("read {path:?}: {error}")))?;
        if n == 0 {
            break;
        }
        hasher.update(raw.as_bytes());
        line_index += 1;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Row {
            source_id: String,
            values: Vec<f64>,
        }
        let row: Row = serde_json::from_str(line).map_err(|error| {
            EmbedError::Artifact(format!("parse {path:?} line {line_index}: {error}"))
        })?;
        if row.source_id.is_empty() {
            return Err(EmbedError::Artifact(format!(
                "embeddings line {line_index}: source_id must not be empty"
            )));
        }
        if !source_ids.insert(row.source_id.clone()) {
            return Err(EmbedError::Artifact(format!(
                "duplicate embedding source_id {:?}",
                row.source_id
            )));
        }
        if row.values.is_empty() {
            return Err(EmbedError::Artifact(format!(
                "embeddings line {line_index}: values must not be empty"
            )));
        }
        if let Some(expected) = dimension {
            if row.values.len() != expected {
                return Err(EmbedError::Artifact(format!(
                    "embeddings line {} carries {} values; earlier rows carry {expected}",
                    line_index,
                    row.values.len()
                )));
            }
        } else {
            dimension = Some(row.values.len());
        }
        for (index, value) in row.values.iter().enumerate() {
            let converted = *value as f32;
            // Reject non-finite input and values that overflow the canonical F32 wire form.
            if !value.is_finite() || !converted.is_finite() {
                return Err(EmbedError::Artifact(format!(
                    "embeddings line {} value {index} is not finite",
                    line_index
                )));
            }
        }
        rows.push(EmbeddingRow {
            source_id: row.source_id,
            values: row.values.iter().map(|value| *value as f32).collect(),
        });
    }
    if rows.is_empty() {
        return Err(EmbedError::Artifact("no embedding rows found".to_owned()));
    }
    Ok((rows, hex_digest(&hasher.finalize())))
}

/// Read the ordered `source_id` stream from the load's vertices NDJSON. Duplicates are
/// rejected: the ordinal-to-row mapping would otherwise be ambiguous.
fn read_vertex_source_ids(path: &Path) -> Result<Vec<String>, EmbedError> {
    let file = fs::File::open(path)
        .map_err(|error| EmbedError::Artifact(format!("read {path:?}: {error}")))?;
    let mut reader = BufReader::new(file);
    let mut ids = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut raw = String::new();
    let mut line_index = 0usize;
    loop {
        raw.clear();
        let n = reader
            .read_line(&mut raw)
            .map_err(|error| EmbedError::Artifact(format!("read {path:?}: {error}")))?;
        if n == 0 {
            break;
        }
        line_index += 1;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        #[derive(Deserialize)]
        struct VertexSourceId {
            #[serde(rename = "source_id")]
            source_id: String,
        }
        let row: VertexSourceId = serde_json::from_str(line).map_err(|error| {
            EmbedError::Artifact(format!("parse {path:?} line {line_index}: {error}"))
        })?;
        if row.source_id.is_empty() {
            return Err(EmbedError::Artifact(format!(
                "vertices line {line_index}: source_id must not be empty"
            )));
        }
        if !seen.insert(row.source_id.clone()) {
            return Err(EmbedError::Artifact(format!(
                "duplicate vertex source_id {:?}",
                row.source_id
            )));
        }
        ids.push(row.source_id);
    }
    Ok(ids)
}

// ──── receipt walk (mapping reconstruction) ────

/// Reconstruct `source_id -> encoded vertex id` from the completed job's receipts.
///
/// Walk receipts in `chunk_index` order and consume exactly `len(allocated_vertex_ids)` rows
/// of the concatenated vertex-row stream per receipt. Edge-only chunks allocate nothing and
/// consume no rows. Fails closed when receipts and the vertex stream disagree (stale or
/// mismatched seeds), or when an embedding names a vertex that received no allocated id.
pub fn resolve_embedding_ids(
    vertex_source_ids: &[String],
    receipts: &[BulkLoadChunkReceiptV1],
    embeddings: &[EmbeddingRow],
) -> Result<Vec<ResolvedEmbedding>, EmbedError> {
    if receipts
        .windows(2)
        .any(|pair| pair[0].chunk_index >= pair[1].chunk_index)
    {
        return Err(EmbedError::Remote(
            "bulk-load receipts arrived out of chunk_index order".into(),
        ));
    }
    let allocated_total: usize = receipts
        .iter()
        .map(|row| row.receipt.allocated_vertex_ids.len())
        .sum();
    if allocated_total != vertex_source_ids.len() {
        return Err(EmbedError::Operator(format!(
            "receipts allocate {allocated_total} vertex ids but the vertices file holds {} rows; \
             the seeds and the load job do not match",
            vertex_source_ids.len(),
        )));
    }

    let mut id_by_source: HashMap<String, Vec<u8>> =
        HashMap::with_capacity(vertex_source_ids.len());
    let mut cursor = 0usize;
    for receipt in receipts {
        for encoded in &receipt.receipt.allocated_vertex_ids {
            let source_id = &vertex_source_ids[cursor];
            id_by_source.insert(source_id.clone(), encoded.clone());
            cursor += 1;
        }
    }

    embeddings
        .iter()
        .map(|row| {
            let encoded = id_by_source.get(&row.source_id).ok_or_else(|| {
                EmbedError::Operator(format!(
                    "embedding source_id {:?} has no allocated vertex id in the completed load",
                    row.source_id
                ))
            })?;
            Ok(ResolvedEmbedding {
                source_id: row.source_id.clone(),
                encoded_vertex_id: encoded.clone(),
                values: row.values.clone(),
            })
        })
        .collect()
}

/// One embedding resolved to its loaded vertex's opaque 8-byte encoded id.
#[derive(Clone, Debug)]
pub struct ResolvedEmbedding {
    pub source_id: String,
    pub encoded_vertex_id: Vec<u8>,
    pub values: Vec<f32>,
}

// ──── batching ────

/// Fit the next candidate batch to the inter-canister payload bound using measured encoded
/// sizes (same policy family as `load`).
fn fitted_batch_sizes(
    total: usize,
    mut measure: impl FnMut(usize) -> Result<usize, EmbedError>,
) -> Result<Vec<usize>, EmbedError> {
    let mut sizes = Vec::new();
    let mut remaining = total;
    let mut hint: Option<SizeHint> = None;
    while remaining > 0 {
        let candidate_window = remaining;
        let fitted = adaptive_fitting_prefix(
            candidate_window,
            hint,
            SizingPolicy::inter_canister(),
            &mut measure,
        )
        .map_err(|error| match error {
            FitError::Measure(error) => error,
            FitError::NoEntryFits { .. } => EmbedError::Artifact(
                "an embedding batch does not fit the payload bound even with one item".into(),
            ),
        })?
        .expect("non-empty candidate window");
        sizes.push(fitted.entry_count);
        hint = Some(SizeHint::new(fitted.entry_count));
        remaining -= fitted.entry_count;
    }
    Ok(sizes)
}

fn encode_batch_measure(
    graph_name: &str,
    embedding_name: &str,
    items: &[ResolvedEmbedding],
) -> Result<usize, EmbedError> {
    let args = AdminIngestVertexEmbeddingBatchArgs {
        logical_graph_name: graph_name.to_owned(),
        embedding_name: embedding_name.to_owned(),
        items: items
            .iter()
            .map(|item| AdminIngestVertexEmbeddingBatchItem {
                encoded_vertex_id: item.encoded_vertex_id.clone(),
                values: item.values.clone(),
            })
            .collect(),
    };
    Encode!(&args)
        .map(|bytes| bytes.len())
        .map_err(|error| EmbedError::Artifact(format!("encode ingest batch: {error}")))
}

/// Outcome of one ingest run over all batches.
#[derive(Debug, Default)]
pub struct IngestSummary {
    pub attempted: usize,
    pub applied: usize,
    pub pending: usize,
    pub failures: Vec<(String, String)>,
}

/// Dispatch every resolved embedding in payload-fitted sequential batches, collecting per-item
/// outcomes. A transport failure aborts immediately; per-item Router verdicts accumulate.
fn ingest_resolved<T: EmbedIngestTransport>(
    resolved: &[ResolvedEmbedding],
    transport: &mut T,
    graph_name: &str,
    embedding_name: &str,
    progress: &mut ProgressLine,
) -> Result<IngestSummary, EmbedError> {
    // Precompute the batch plan once so the progress denominator is exact.
    let measure = |count: usize| -> Result<usize, EmbedError> {
        encode_batch_measure(graph_name, embedding_name, &resolved[..count])
    };
    let sizes = fitted_batch_sizes(resolved.len(), measure)?;

    let mut summary = IngestSummary::default();
    let mut offset = 0usize;
    for size in sizes {
        let batch = &resolved[offset..offset + size];
        offset += size;
        let args = AdminIngestVertexEmbeddingBatchArgs {
            logical_graph_name: graph_name.to_owned(),
            embedding_name: embedding_name.to_owned(),
            items: batch
                .iter()
                .map(|item| AdminIngestVertexEmbeddingBatchItem {
                    encoded_vertex_id: item.encoded_vertex_id.clone(),
                    values: item.values.clone(),
                })
                .collect(),
        };
        let results = transport
            .ingest_batch(&args)
            .map_err(EmbedError::Remote)?
            .map_err(|error| {
                EmbedError::Remote(format!("Router rejected ingest_vertex_embeddings: {error}"))
            })?;
        if results.len() != batch.len() {
            return Err(EmbedError::Remote(format!(
                "ingest_vertex_embeddings returned {} results for {} items",
                results.len(),
                batch.len()
            )));
        }
        for (item, outcome) in batch.iter().zip(results) {
            summary.attempted += 1;
            match outcome {
                Ok(result) => match result.projection_outcome {
                    VertexEmbeddingProjectionOutcome::Applied => summary.applied += 1,
                    VertexEmbeddingProjectionOutcome::Pending => summary.pending += 1,
                },
                Err(reason) => summary.failures.push((item.source_id.clone(), reason)),
            }
        }
        let percent = (summary.attempted.saturating_mul(100) / resolved.len()) as u8;
        progress.render(
            percent,
            &format!("embedding {} / {}", summary.attempted, resolved.len()),
        );
    }
    Ok(summary)
}

// ──── entry point ────

/// Page the full receipt list of the job under `key`, requiring a terminal Completed state.
fn completed_receipts<T: EmbedIngestTransport>(
    transport: &mut T,
    graph: Option<&str>,
    key: &str,
) -> Result<Vec<BulkLoadChunkReceiptV1>, EmbedError> {
    let mut receipts = Vec::new();
    let mut cursor: Option<u32> = None;
    let page: BulkLoadStatusPage = loop {
        let result = transport
            .status(graph, key, cursor, STATUS_PAGE_SIZE)
            .map_err(EmbedError::Remote)?;
        let next = match result {
            Ok(page) => page,
            Err(RouterError::NotFound(_)) => {
                return Err(EmbedError::Operator(format!(
                    "no bulk-load job exists under --key {key}; run `gleaph load` first"
                )));
            }
            Err(error) => {
                return Err(EmbedError::Remote(format!(
                    "bulk_load_status rejected: {error}"
                )));
            }
        };
        receipts.extend(next.receipts.iter().cloned());
        match next.next_receipt_cursor {
            Some(next_cursor) => cursor = Some(next_cursor),
            None => break next,
        }
    };
    match page.state {
        BulkLoadPublicStateV1::Completed => Ok(receipts),
        BulkLoadPublicStateV1::Failed { reason } => Err(EmbedError::Operator(format!(
            "bulk-load job under --key {key} failed: {reason}"
        ))),
        BulkLoadPublicStateV1::Aborted => Err(EmbedError::Operator(format!(
            "bulk-load job under --key {key} was aborted"
        ))),
        other => Err(EmbedError::Operator(format!(
            "bulk-load job under --key {key} is still {other:?}; embeddings require a Completed job"
        ))),
    }
}

/// Everything one ingest run needs besides the transport.
pub struct IngestPlan<'a> {
    pub graph_name: &'a str,
    pub embedding_name: &'a str,
    pub key: &'a str,
    pub vertices_path: &'a Path,
    pub embeddings_path: &'a Path,
}

/// Validate inputs, reconstruct the id mapping from the completed job's receipts, and dispatch
/// every embedding through `transport`. This is the full CLI ingest path minus connection
/// setup, so integration tests can inject a direct-call transport.
pub fn run_with_transport<T: EmbedIngestTransport>(
    plan: &IngestPlan<'_>,
    transport: &mut T,
) -> Result<IngestSummary, EmbedError> {
    // Everything below happens only after full validation and digestion of both inputs.
    let (embeddings, _digest) = scan_embeddings(plan.embeddings_path)?;
    let vertex_source_ids = read_vertex_source_ids(plan.vertices_path)?;
    let receipts = completed_receipts(transport, Some(plan.graph_name), plan.key)?;
    let resolved = resolve_embedding_ids(&vertex_source_ids, &receipts, &embeddings)?;
    let tty = std::io::stdout().is_terminal();
    let mut progress = ProgressLine::new(tty);
    let summary = ingest_resolved(
        &resolved,
        transport,
        plan.graph_name,
        plan.embedding_name,
        &mut progress,
    )?;
    progress.close();
    Ok(summary)
}

/// Validate and run one `gleaph embed ingest` invocation.
pub fn execute(args: &EmbedIngestArgs) -> Result<IngestSummary, EmbedError> {
    let canister = args
        .canister
        .as_deref()
        .ok_or_else(|| EmbedError::Usage("--canister is required".into()))?;
    if args.embedding_name.is_empty() {
        return Err(EmbedError::Usage(
            "--embedding-name must not be empty".into(),
        ));
    }
    let graph_name = args.graph.as_deref().ok_or_else(|| {
        EmbedError::Usage(
            "--graph is required: the vector index resolves through the logical graph name".into(),
        )
    })?;
    let key = effective_key(args)?;

    let mut transport = RouterEmbedTransport::connect(
        canister,
        args.network
            .as_deref()
            .unwrap_or(crate::config::DEFAULT_NETWORK),
        args.identity.as_deref(),
        args.fetch_root_key.unwrap_or(false),
    )?;

    let plan = IngestPlan {
        graph_name,
        embedding_name: &args.embedding_name,
        key: &key,
        vertices_path: &args.vertices,
        embeddings_path: &args.embeddings,
    };
    run_with_transport(&plan, &mut transport)
}

/// The base job key: the explicit `--key` or the built-in default shared with `load`.
fn base_key(args: &EmbedIngestArgs) -> &str {
    args.key.as_deref().unwrap_or(crate::load::DEFAULT_BULK_KEY)
}

/// Resolve the effective job key: the state-file record when present (the same resume/skip
/// pointer `load` writes), else the explicit `--key`, else the default.
fn effective_key(args: &EmbedIngestArgs) -> Result<String, EmbedError> {
    if let Some(path) = &args.state_file {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(base_key(args).to_owned());
            }
            Err(error) => {
                return Err(EmbedError::Usage(format!(
                    "state file {path:?} is unreadable: {error}"
                )));
            }
        };
        #[derive(Deserialize)]
        struct StateKeyOnly {
            bulk_key: String,
        }
        let state: StateKeyOnly = serde_json::from_slice(&bytes).map_err(|error| {
            EmbedError::Usage(format!("state file {path:?} is invalid: {error}"))
        })?;
        return Ok(state.bulk_key);
    }
    Ok(base_key(args).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_bulk_load_api::AtomicInsertReceiptV1;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gleaph-cli-embed-{}-{nonce}-{tag}",
            std::process::id()
        ))
    }

    fn embedding_row(source_id: &str, values: &[f32]) -> EmbeddingRow {
        EmbeddingRow {
            source_id: source_id.to_owned(),
            values: values.to_vec(),
        }
    }

    /// One vertex-allocating receipt committing `ids` (each id one distinct byte vector).
    fn vertex_receipt(chunk_index: u32, ids: &[[u8; 8]]) -> BulkLoadChunkReceiptV1 {
        BulkLoadChunkReceiptV1 {
            chunk_index,
            receipt: AtomicInsertReceiptV1 {
                logical_operation_count: ids.len() as u64,
                logical_vertex_count: ids.len() as u64,
                logical_edge_count: 0,
                allocated_vertex_ids: ids.iter().map(|id| id.to_vec()).collect(),
            },
        }
    }

    /// One edge-only receipt: it allocates nothing and consumes no vertex rows.
    fn edge_receipt(chunk_index: u32) -> BulkLoadChunkReceiptV1 {
        BulkLoadChunkReceiptV1 {
            chunk_index,
            receipt: AtomicInsertReceiptV1 {
                logical_operation_count: 1,
                logical_vertex_count: 0,
                logical_edge_count: 1,
                allocated_vertex_ids: Vec::new(),
            },
        }
    }

    fn vertices() -> Vec<String> {
        ["a", "b", "c", "d"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    fn sample_embeddings() -> Vec<EmbeddingRow> {
        vec![
            embedding_row("a", &[0.25, -0.5]),
            embedding_row("d", &[1.0, 2.0]),
        ]
    }

    #[test]
    fn mapping_reconstruction_survives_prefix_committed_and_edge_only_chunks() {
        // Router committed a budget-fitting *prefix* of chunk 0 (2 of 3 proposed rows), then an
        // edge-only chunk, then the remaining two vertex rows. Receipts are the ground truth,
        // so ordinal order — not proposal order — must drive the mapping.
        let id = |seed: u8| [seed; 8];
        let receipts = vec![
            vertex_receipt(0, &[id(1), id(2)]),
            edge_receipt(1),
            vertex_receipt(2, &[id(3), id(4)]),
        ];
        let resolved =
            resolve_embedding_ids(&vertices(), &receipts, &sample_embeddings()).expect("resolve");
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].source_id, "a");
        assert_eq!(resolved[0].encoded_vertex_id, id(1).to_vec());
        assert_eq!(resolved[1].source_id, "d");
        assert_eq!(resolved[1].encoded_vertex_id, id(4).to_vec());
        assert_eq!(resolved[0].values, vec![0.25, -0.5]);
    }

    #[test]
    fn allocation_count_mismatch_fails_closed() {
        // Only three of four rows received ids: a stale/mismatched seed set must be rejected
        // before any remote call rather than silently dropping the tail row.
        let receipts = vec![vertex_receipt(0, &[[1; 8], [2; 8], [3; 8]])];
        let error = resolve_embedding_ids(&vertices(), &receipts, &sample_embeddings())
            .expect_err("mismatched totals must fail");
        assert!(
            error.to_string().contains("do not match"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn out_of_order_receipts_fail_closed() {
        let receipts = vec![vertex_receipt(2, &[[1; 8]]), vertex_receipt(0, &[[2; 8]])];
        let error = resolve_embedding_ids(&["a".to_owned()], &receipts, &[])
            .expect_err("unordered receipts must fail");
        assert!(
            error.to_string().contains("chunk_index order"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn missing_embedding_source_id_fails_closed() {
        let receipts = vec![vertex_receipt(0, &[[1; 8], [2; 8], [3; 8], [4; 8]])];
        let embeddings = vec![embedding_row("not-loaded", &[1.0])];
        let error = resolve_embedding_ids(&vertices(), &receipts, &embeddings)
            .expect_err("unknown embedding source must fail");
        assert!(
            error
                .to_string()
                .contains(r#""not-loaded" has no allocated vertex id"#),
            "unexpected error: {error}"
        );
    }

    fn write_embeddings(path: &Path, body: &str) -> PathBuf {
        fs::write(path, body).expect("write embeddings fixture");
        path.to_owned()
    }

    #[test]
    fn scan_rejects_duplicate_source_ids_ragged_rows_and_non_finite_values() {
        // Duplicate source_id.
        let path = write_embeddings(
            &temp_path("dup"),
            r#"{"source_id":"a","values":[1.0]}
{"source_id":"a","values":[2.0]}
"#,
        );
        let error = scan_embeddings(&path).expect_err("duplicate must fail");
        assert!(error.to_string().contains("duplicate embedding source_id"));

        // Ragged row lengths across rows.
        let path = write_embeddings(
            &temp_path("ragged"),
            r#"{"source_id":"a","values":[1.0,2.0]}
{"source_id":"b","values":[1.0]}
"#,
        );
        let error = scan_embeddings(&path).expect_err("ragged lengths must fail");
        assert!(
            error.to_string().contains("earlier rows carry 2"),
            "unexpected error: {error}"
        );

        // NaN is not valid JSON, so it fails at parse time; f64-parsed infinities fail the
        // finite check.
        let path = write_embeddings(&temp_path("nan"), r#"{"source_id":"a","values":[NaN]}"#);
        let error = scan_embeddings(&path).expect_err("NaN must fail");
        assert!(
            error.to_string().contains("parse"),
            "NaN must be rejected as unparsable input, got: {error}"
        );

        // Overflow beyond canonical F32 range.
        let path = write_embeddings(
            &temp_path("overflow"),
            r#"{"source_id":"a","values":[1e40]}"#,
        );
        let error = scan_embeddings(&path).expect_err("f64 overflow into f32 must fail");
        assert!(error.to_string().contains("not finite"));

        // f64 exponent overflow is rejected by the strict JSON number grammar itself.
        let path = write_embeddings(&temp_path("inf"), r#"{"source_id":"a","values":[1e400]}"#);
        let error = scan_embeddings(&path).expect_err("out-of-range value must fail");
        assert!(
            error.to_string().contains("out of range"),
            "unexpected error: {error}"
        );

        // Empty values list.
        let path = write_embeddings(&temp_path("empty"), r#"{"source_id":"a","values":[]}"#);
        let error = scan_embeddings(&path).expect_err("empty values must fail");
        assert!(error.to_string().contains("must not be empty"));
    }

    #[test]
    fn scan_digests_the_raw_bytes_for_state_identity() {
        let path = write_embeddings(
            &temp_path("digest"),
            "{\"source_id\":\"a\",\"values\":[1.0]}\n",
        );
        let (rows, digest) = scan_embeddings(&path).expect("scan");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values, vec![1.0f32]);
        assert_eq!(digest.len(), 64, "digest must be sha256 hex");
    }

    #[test]
    fn batch_plan_splits_under_a_tiny_payload_bound() {
        // A bound where two items never fit forces single-item batches; a roomy bound keeps
        // everything in one call.
        let tiny = |_count: usize| -> Result<usize, EmbedError> {
            Ok(if _count >= 2 { usize::MAX / 2 } else { 128 })
        };
        assert_eq!(
            fitted_batch_sizes(4, tiny).expect("tiny plan"),
            vec![1, 1, 1, 1],
            "the wrong implementation that ignores the measured bound would emit one batch"
        );

        let roomy = |count: usize| -> Result<usize, EmbedError> { Ok(count * 64) };
        assert_eq!(fitted_batch_sizes(4, roomy).expect("roomy plan"), vec![4]);
    }

    struct FakeEmbedTransport {
        status_pages: std::collections::VecDeque<Result<BulkLoadStatusPage, RouterError>>,
        ingest_verdicts: std::collections::VecDeque<
            Result<Vec<Result<VertexEmbeddingIngestionResult, String>>, RouterError>,
        >,
        status_calls: Vec<String>,
        ingested_batches: Vec<Vec<String>>,
    }

    impl FakeEmbedTransport {
        fn completed_page(receipts: Vec<BulkLoadChunkReceiptV1>) -> BulkLoadStatusPage {
            BulkLoadStatusPage {
                state: BulkLoadPublicStateV1::Completed,
                next_chunk_index: receipts.len() as u32,
                committed_chunk_count: receipts.len() as u32,
                completed_chunk_count: receipts.len() as u32,
                terminal_at_ns: Some(1),
                expires_at_ns: None,
                next_receipt_cursor: None,
                receipts,
            }
        }
    }

    impl EmbedIngestTransport for FakeEmbedTransport {
        fn status(
            &mut self,
            graph: Option<&str>,
            key: &str,
            cursor: Option<u32>,
            max_receipts: u32,
        ) -> Result<Result<BulkLoadStatusPage, RouterError>, String> {
            self.status_calls
                .push(format!("status:{graph:?}:{key}:{cursor:?}:{max_receipts}"));
            Ok(self.status_pages.pop_front().expect("queue a status page"))
        }

        fn ingest_batch(
            &mut self,
            args: &AdminIngestVertexEmbeddingBatchArgs,
        ) -> Result<Result<Vec<Result<VertexEmbeddingIngestionResult, String>>, RouterError>, String>
        {
            self.ingested_batches.push(
                args.items
                    .iter()
                    .map(|item| String::from_utf8_lossy(&item.encoded_vertex_id).into_owned())
                    .collect(),
            );
            Ok(self
                .ingest_verdicts
                .pop_front()
                .expect("queue an ingest verdict"))
        }
    }

    #[test]
    fn non_terminal_job_fails_closed_before_any_ingest_call() {
        let mut transport = FakeEmbedTransport {
            status_pages: std::collections::VecDeque::from([Ok(BulkLoadStatusPage {
                state: BulkLoadPublicStateV1::FinalizePending,
                ..FakeEmbedTransport::completed_page(Vec::new())
            })]),
            ingest_verdicts: std::collections::VecDeque::new(),
            status_calls: Vec::new(),
            ingested_batches: Vec::new(),
        };
        let error = completed_receipts(&mut transport, Some("knowledge"), "job-key")
            .expect_err("a running job must not admit ingestion");
        assert!(
            error.to_string().contains("still FinalizePending"),
            "unexpected error: {error}"
        );
        assert!(
            transport.ingested_batches.is_empty(),
            "no embedding may be dispatched against a non-terminal job"
        );
    }
}
