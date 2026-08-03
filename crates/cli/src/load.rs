//! `gleaph load`: load initial vertices and edges into an existing logical graph through the
//! durable Router `bulk_load` lifecycle (ADR 0057, ADR 0060 Decision 4).
//!
//! The artifact is a YAML/JSON single file (`format_version: 1`, `vertices` + `edges`) or two
//! NDJSON files (`vertices.jsonl` + `edges.jsonl`). The CLI validates everything before any remote
//! call, then drives the lifecycle: `Start` → vertex chunks → edge chunks (endpoints resolved to
//! the encoded vertex IDs allocated by the Router) → `Finalize` → poll `Completed`. Chunk
//! boundaries are Router-owned (`Resumable` execution commits a budget-fitting prefix and returns
//! `next_offset`); the CLI only fits each request to the ingress payload bound and loops.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use candid::Encode;
use clap::Args;
use gleaph_gql::value::Value;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_message_sizing::{FitError, SizeHint, SizingPolicy, adaptive_fitting_prefix};
use gleaph_router::types::{
    AtomicInsertPropertyV1, AtomicInsertVertexV1, BulkLoadChunkReceiptV1, BulkLoadChunkV1,
    BulkLoadCommand, BulkLoadEdgeV1, BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::remote::RemoteTransport;

const FORMAT_VERSION: u32 = 1;
/// Bounded single-file input. Larger artifacts must use NDJSON, which is the streaming family.
/// The cap also bounds YAML alias-expansion work for untrusted artifacts.
const MAX_SINGLE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_BULK_KEY: &str = "initial-load-v1";
const STATUS_PAGE_SIZE: u32 = 64;
const MAX_FINALIZE_POLL_ATTEMPTS: usize = 120;
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Artifact families understood by `gleaph load`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Yaml,
    Json,
    Jsonl,
}

/// `gleaph load` command-line arguments (mirrors the `migration` remote conventions).
#[derive(Debug, Args)]
pub struct LoadArgs {
    /// One YAML/JSON artifact, or the vertices and edges NDJSON files.
    #[arg(value_name = "ARTIFACT")]
    artifacts: Vec<PathBuf>,
    /// Router canister principal.
    #[arg(long, value_name = "PRINCIPAL")]
    canister: String,
    /// Logical graph name; omitted for the caller's default (HOME) graph.
    #[arg(long, value_name = "NAME")]
    graph: Option<String>,
    /// Durable bulk-load job key. Terminal jobs are single-use: a completed/failed/aborted key
    /// cannot be reused, so re-loading after a terminal state requires a new key or `--fresh`.
    #[arg(short = 'k', long, default_value = DEFAULT_BULK_KEY, value_name = "KEY")]
    key: String,
    /// Network name (ic/local) or an HTTP(S) endpoint URL.
    #[arg(short = 'n', long, default_value = "ic", value_name = "NETWORK")]
    network: String,
    /// PEM file containing a Secp256k1 identity.
    #[arg(long, value_name = "PATH")]
    identity: Option<PathBuf>,
    /// Fetch the network root key before querying a custom endpoint.
    #[arg(long)]
    fetch_root_key: bool,
    /// Artifact format; inferred from the file extension when omitted.
    #[arg(long, value_name = "FORMAT")]
    format: Option<Format>,
    /// Start a new job under a derived key instead of resuming or skipping; the effective key is
    /// printed and recorded in `--state-file` when given.
    #[arg(long)]
    fresh: bool,
    /// State file recording the effective job key and the loaded artifact digest. When present,
    /// the recorded key is reused (resume/skip identity) and skip-on-Completed requires the
    /// digest to match.
    #[arg(long, value_name = "PATH")]
    state_file: Option<PathBuf>,
}

/// Failures raised by `gleaph load`, mapped to the documented exit codes:
/// 1 operator action, 2 input validation, 3 remote/auth.
#[derive(Debug, Error)]
pub enum LoadError {
    #[error("{0}")]
    Usage(String),
    #[error("{0}")]
    Artifact(String),
    #[error("{0}")]
    Remote(String),
    #[error("{0}")]
    Operator(String),
}

impl LoadError {
    pub fn exit_code(&self) -> u8 {
        match self {
            LoadError::Usage(_) | LoadError::Artifact(_) => 2,
            LoadError::Remote(_) => 3,
            LoadError::Operator(_) => 1,
        }
    }
}

/// Terminal result of one load invocation.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    Loaded { key: String },
    Skipped { key: String },
}

// ──── artifact model ────

struct LoadArtifact {
    vertices: Vec<VertexRow>,
    edges: Vec<EdgeRow>,
}

/// Order-preserving property map with duplicate-name rejection (the wire requires unique names).
#[derive(Default)]
struct Properties(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(PropertiesVisitor)
    }
}

struct PropertiesVisitor;

impl<'de> Visitor<'de> for PropertiesVisitor {
    type Value = Properties;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "a property object with unique field names")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Properties, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Vec::new();
        let mut seen = HashSet::new();
        while let Some(name) = map.next_key::<String>()? {
            if !seen.insert(name.clone()) {
                return Err(de::Error::custom(format!("duplicate property {name:?}")));
            }
            fields.push((name, map.next_value::<Value>()?));
        }
        Ok(Properties(fields))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VertexRow {
    source_id: String,
    labels: Vec<String>,
    #[serde(default)]
    properties: Properties,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeRow {
    /// Source vertex `source_id`.
    source: String,
    /// Target vertex `source_id`.
    target: String,
    label: String,
    #[serde(default = "default_directed")]
    directed: bool,
    #[serde(default)]
    inline_value: Option<Value>,
    #[serde(default)]
    properties: Properties,
}

fn default_directed() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SingleFileArtifact {
    format_version: u32,
    #[serde(default)]
    vertices: Vec<VertexRow>,
    #[serde(default)]
    edges: Vec<EdgeRow>,
}

// ──── format detection and reading ────

fn resolve_format(args: &LoadArgs) -> Result<Format, LoadError> {
    let count = args.artifacts.len();
    if let Some(format) = args.format {
        let expected = match format {
            Format::Jsonl => 2,
            Format::Yaml | Format::Json => 1,
        };
        if count != expected {
            return Err(LoadError::Usage(format!(
                "--format {format:?} expects {expected} artifact file(s), got {count}"
            )));
        }
        return Ok(format);
    }
    match count {
        1 => match extension(&args.artifacts[0])?.as_str() {
            "json" => Ok(Format::Json),
            "yaml" | "yml" => Ok(Format::Yaml),
            "jsonl" | "ndjson" => Err(LoadError::Usage(
                "NDJSON artifacts require two files: <VERTICES> <EDGES>".into(),
            )),
            other => Err(LoadError::Usage(format!(
                "cannot infer artifact format from extension {other:?}; pass --format yaml|json|jsonl"
            ))),
        },
        2 => {
            for path in &args.artifacts {
                let ext = extension(path)?;
                if !matches!(ext.as_str(), "jsonl" | "ndjson") {
                    return Err(LoadError::Usage(format!(
                        "two artifact files require NDJSON (.jsonl); got extension {ext:?}"
                    )));
                }
            }
            Ok(Format::Jsonl)
        }
        other => Err(LoadError::Usage(format!(
            "expected one YAML/JSON artifact or two NDJSON artifacts, got {other}"
        ))),
    }
}

fn extension(path: &Path) -> Result<String, LoadError> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| LoadError::Usage(format!("artifact path {path:?} has no extension")))
}

fn read_artifact(paths: &[PathBuf], format: Format) -> Result<LoadArtifact, LoadError> {
    match format {
        Format::Json | Format::Yaml => read_single_file(&paths[0], format),
        Format::Jsonl => read_jsonl(&paths[0], &paths[1]),
    }
}

fn read_single_file(path: &Path, format: Format) -> Result<LoadArtifact, LoadError> {
    let metadata = fs::metadata(path)
        .map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
    if metadata.len() > MAX_SINGLE_FILE_BYTES {
        return Err(LoadError::Artifact(format!(
            "artifact {path:?} exceeds the {} MiB single-file bound; use NDJSON (.jsonl) for larger data",
            MAX_SINGLE_FILE_BYTES / (1024 * 1024)
        )));
    }
    let bytes =
        fs::read(path).map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| LoadError::Artifact(format!("artifact {path:?} is not UTF-8")))?;
    let parsed: SingleFileArtifact = match format {
        Format::Json => serde_json::from_str(text)
            .map_err(|error| LoadError::Artifact(format!("parse {path:?}: {error}")))?,
        Format::Yaml => serde_yml::from_str(text)
            .map_err(|error| LoadError::Artifact(format!("parse {path:?}: {error}")))?,
        Format::Jsonl => unreachable!("single-file reader is only used for YAML/JSON"),
    };
    if parsed.format_version != FORMAT_VERSION {
        return Err(LoadError::Artifact(format!(
            "unsupported format_version {}; expected {FORMAT_VERSION}",
            parsed.format_version
        )));
    }
    Ok(LoadArtifact {
        vertices: parsed.vertices,
        edges: parsed.edges,
    })
}

fn read_jsonl(vertices_path: &Path, edges_path: &Path) -> Result<LoadArtifact, LoadError> {
    Ok(LoadArtifact {
        vertices: read_jsonl_rows(vertices_path)?,
        edges: read_jsonl_rows(edges_path)?,
    })
}

fn read_jsonl_rows<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Vec<T>, LoadError> {
    let text = fs::read_to_string(path)
        .map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
    let mut rows = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: T = serde_json::from_str(line).map_err(|error| {
            LoadError::Artifact(format!("parse {path:?} line {}: {error}", line_index + 1))
        })?;
        rows.push(row);
    }
    Ok(rows)
}

// ──── validation ────

fn validate_artifact(
    artifact: &LoadArtifact,
    graph: Option<&str>,
    key: &str,
) -> Result<(), LoadError> {
    if let Some(name) = graph
        && (name.is_empty() || name.len() > 256)
    {
        return Err(LoadError::Usage(
            "--graph must be 1..=256 UTF-8 bytes".into(),
        ));
    }
    if key.is_empty() || key.len() > 256 {
        return Err(LoadError::Usage("--key must be 1..=256 UTF-8 bytes".into()));
    }
    let mut source_ids = HashSet::new();
    for (index, vertex) in artifact.vertices.iter().enumerate() {
        if vertex.source_id.is_empty() {
            return Err(LoadError::Artifact(format!(
                "vertices[{index}] source_id must not be empty"
            )));
        }
        if !source_ids.insert(vertex.source_id.clone()) {
            return Err(LoadError::Artifact(format!(
                "duplicate vertex source_id {:?}",
                vertex.source_id
            )));
        }
        if vertex.labels.is_empty() || vertex.labels.iter().any(String::is_empty) {
            return Err(LoadError::Artifact(format!(
                "vertices[{index}] requires non-empty labels"
            )));
        }
        validate_properties(index, &vertex.properties)?;
    }
    for (index, edge) in artifact.edges.iter().enumerate() {
        if edge.source.is_empty() || edge.target.is_empty() {
            return Err(LoadError::Artifact(format!(
                "edges[{index}] source/target must not be empty"
            )));
        }
        if !source_ids.contains(&edge.source) || !source_ids.contains(&edge.target) {
            return Err(LoadError::Artifact(format!(
                "edges[{index}] endpoint does not resolve to a vertex source_id"
            )));
        }
        if edge.label.is_empty() {
            return Err(LoadError::Artifact(format!(
                "edges[{index}] label must not be empty"
            )));
        }
        validate_properties(index, &edge.properties)?;
    }
    Ok(())
}

fn validate_properties(index: usize, properties: &Properties) -> Result<(), LoadError> {
    let mut names = HashSet::new();
    for (name, _) in &properties.0 {
        if name.is_empty() {
            return Err(LoadError::Artifact(format!(
                "row {index} contains an empty property name"
            )));
        }
        if !names.insert(name.clone()) {
            return Err(LoadError::Artifact(format!(
                "row {index} contains a duplicate property name {name:?}"
            )));
        }
    }
    Ok(())
}

// ──── digest and state file ────

fn artifact_digest(paths: &[PathBuf], format: Format) -> Result<String, LoadError> {
    let mut hasher = Sha256::new();
    match format {
        Format::Json | Format::Yaml => {
            let bytes = fs::read(&paths[0])
                .map_err(|error| LoadError::Artifact(format!("read {:?}: {error}", paths[0])))?;
            hasher.update(bytes);
        }
        Format::Jsonl => {
            for path in paths {
                let bytes = fs::read(path)
                    .map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
                hasher.update(bytes);
            }
        }
    }
    Ok(hex_digest(&hasher.finalize()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[derive(Serialize, Deserialize)]
struct LoadState {
    format_version: u32,
    artifact_sha256: String,
    bulk_key: String,
    graph: Option<String>,
}

fn read_state_file(path: &Path) -> Result<Option<LoadState>, LoadError> {
    let Ok(bytes) = fs::read(path) else {
        return Ok(None);
    };
    let state: LoadState = serde_json::from_slice(&bytes)
        .map_err(|error| LoadError::Usage(format!("state file {path:?} is invalid: {error}")))?;
    if state.format_version != FORMAT_VERSION {
        return Err(LoadError::Usage(format!(
            "state file {path:?} has unsupported format_version {}",
            state.format_version
        )));
    }
    Ok(Some(state))
}

fn write_state_file(
    path: &Path,
    digest: &str,
    key: &str,
    graph: Option<&str>,
) -> Result<(), LoadError> {
    let state = LoadState {
        format_version: FORMAT_VERSION,
        artifact_sha256: digest.to_owned(),
        bulk_key: key.to_owned(),
        graph: graph.map(str::to_owned),
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| LoadError::Usage(format!("encode state file: {error}")))?;
    fs::write(path, bytes)
        .map_err(|error| LoadError::Operator(format!("write state file {path:?}: {error}")))
}

// ──── transport ────

/// Durable bulk-load boundary owned by the CLI. The fake in tests implements the same contract.
trait BulkLoadTransport {
    /// One `bulk_load_status` page. `Err(NotFound)` means no job exists under the key.
    fn status(
        &mut self,
        graph: Option<&str>,
        key: &str,
        cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<Result<BulkLoadStatusPage, RouterError>, String>;

    fn command(&mut self, command: BulkLoadCommand) -> Result<BulkLoadResponse, String>;
}

struct RemoteBulkLoadTransport {
    remote: RemoteTransport,
}

impl RemoteBulkLoadTransport {
    fn connect(
        canister: &str,
        network: &str,
        identity: Option<&Path>,
        fetch_root_key: bool,
    ) -> Result<Self, LoadError> {
        let remote = RemoteTransport::connect(canister, network, identity, fetch_root_key)
            .map_err(LoadError::Remote)?;
        Ok(Self { remote })
    }
}

impl BulkLoadTransport for RemoteBulkLoadTransport {
    fn status(
        &mut self,
        graph: Option<&str>,
        key: &str,
        cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<Result<BulkLoadStatusPage, RouterError>, String> {
        let args = (
            graph.map(str::to_owned),
            key.to_owned(),
            cursor,
            max_receipts,
        );
        self.remote.query("bulk_load_status", &args)
    }

    fn command(&mut self, command: BulkLoadCommand) -> Result<BulkLoadResponse, String> {
        match self
            .remote
            .update::<BulkLoadResponse, RouterError>("bulk_load", &command)?
        {
            Ok(response) => Ok(response),
            Err(error) => Err(format!("Router rejected bulk_load: {error:?}")),
        }
    }
}

// ──── loader ────

struct Resume {
    committed_vertices: usize,
    committed_edges: usize,
    next_chunk_index: u32,
    encoded_ids: Vec<Vec<u8>>,
}

fn status_paged(
    transport: &mut impl BulkLoadTransport,
    graph: Option<&str>,
    key: &str,
) -> Result<Option<(BulkLoadStatusPage, Vec<BulkLoadChunkReceiptV1>)>, LoadError> {
    let mut receipts = Vec::new();
    let mut cursor = None;
    let page = loop {
        let result = transport
            .status(graph, key, cursor, STATUS_PAGE_SIZE)
            .map_err(LoadError::Remote)?;
        let next = match result {
            Ok(page) => page,
            Err(RouterError::NotFound(_)) => return Ok(None),
            Err(error) => {
                return Err(LoadError::Remote(format!(
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
    Ok(Some((page, receipts)))
}

fn resume_point(page: &BulkLoadStatusPage, receipts: &[BulkLoadChunkReceiptV1]) -> Resume {
    let mut committed_vertices = 0usize;
    let mut committed_edges = 0usize;
    let mut encoded_ids = Vec::new();
    for row in receipts {
        let receipt = &row.receipt;
        committed_vertices += receipt.logical_vertex_count as usize;
        committed_edges += receipt.logical_edge_count as usize;
        if receipt.logical_vertex_count > 0 {
            encoded_ids.extend(receipt.allocated_vertex_ids.iter().cloned());
        }
    }
    Resume {
        committed_vertices,
        committed_edges,
        next_chunk_index: page.next_chunk_index,
        encoded_ids,
    }
}

/// Drive one durable bulk-load job to `Completed` (ADR 0060 §3 client loop).
fn run_load(
    transport: &mut impl BulkLoadTransport,
    artifact: &LoadArtifact,
    graph: Option<&str>,
    key: &str,
    digest: &str,
    state_file: Option<&Path>,
) -> Result<LoadOutcome, LoadError> {
    if let Some((page, _)) = status_paged(transport, graph, key)? {
        match &page.state {
            BulkLoadPublicStateV1::Completed => {
                if let Some(path) = state_file
                    && let Some(state) = read_state_file(path)?
                    && state.artifact_sha256 != digest
                {
                    return Err(LoadError::Operator(format!(
                        "artifact digest changed since the completed load under --key {key}; use a new --key or --fresh"
                    )));
                }
                return Ok(LoadOutcome::Skipped {
                    key: key.to_owned(),
                });
            }
            BulkLoadPublicStateV1::Failed { reason } => {
                return Err(LoadError::Operator(format!(
                    "bulk-load job under --key {key} failed: {reason}; use a new --key"
                )));
            }
            BulkLoadPublicStateV1::Aborted => {
                return Err(LoadError::Operator(format!(
                    "bulk-load job under --key {key} was aborted; use a new --key"
                )));
            }
            _ => {}
        }
    }

    // Record the effective key and digest before any chunk commits so an interrupted load keeps
    // its resume identity (the state file is the skip/resume pointer on later runs).
    if let Some(path) = state_file {
        write_state_file(path, digest, key, graph)?;
    }

    let started = transport
        .command(BulkLoadCommand::Start {
            graph_name: graph.map(str::to_owned),
            client_bulk_key: key.to_owned(),
        })
        .map_err(LoadError::Remote)?;
    match started {
        BulkLoadResponse::Started { .. } => {}
        other => {
            return Err(LoadError::Remote(format!(
                "unexpected Start response: {other:?}"
            )));
        }
    }

    let (page, receipts) = status_paged(transport, graph, key)?
        .ok_or_else(|| LoadError::Remote("bulk-load job disappeared after Start".into()))?;
    let resume = resume_point(&page, &receipts);
    let mut encoded_ids = resume.encoded_ids;
    let mut chunk_index = resume.next_chunk_index;

    // Vertex phase: append fitted candidates; each Append commits a budget-fitting prefix.
    let mut offset = resume.committed_vertices;
    let mut hint: Option<SizeHint> = None;
    while offset < artifact.vertices.len() {
        let candidate_count = fit_candidate(artifact.vertices.len() - offset, hint, |count| {
            let chunk = vertex_chunk(&artifact.vertices[offset..offset + count])?;
            encode_append_command(graph, key, chunk_index, chunk)
        })?;
        let chunk = vertex_chunk(&artifact.vertices[offset..offset + candidate_count])?;
        let response = transport
            .command(BulkLoadCommand::Append {
                graph_name: graph.map(str::to_owned),
                client_bulk_key: key.to_owned(),
                chunk_index,
                chunk,
            })
            .map_err(LoadError::Remote)?;
        let (next_offset, receipt) = match response {
            BulkLoadResponse::Appended {
                next_offset,
                receipt,
                ..
            } => (next_offset, receipt),
            other => {
                return Err(LoadError::Remote(format!(
                    "unexpected Append response: {other:?}"
                )));
            }
        };
        if next_offset == 0 {
            return Err(LoadError::Remote(
                "bulk-load Append committed zero operations".into(),
            ));
        }
        encoded_ids.extend(receipt.allocated_vertex_ids);
        offset += next_offset as usize;
        chunk_index += 1;
        hint = Some(SizeHint::new(candidate_count));
    }

    // Edge phase: resolve endpoints to the encoded vertex IDs allocated in vertex order.
    let mut id_by_source: HashMap<String, Vec<u8>> =
        HashMap::with_capacity(artifact.vertices.len());
    for (row, id) in artifact.vertices.iter().zip(encoded_ids.iter()) {
        id_by_source.insert(row.source_id.clone(), id.clone());
    }
    let mut offset = resume.committed_edges;
    let mut hint: Option<SizeHint> = None;
    while offset < artifact.edges.len() {
        let candidate_count = fit_candidate(artifact.edges.len() - offset, hint, |count| {
            let chunk = edge_chunk(&artifact.edges[offset..offset + count], &id_by_source)?;
            encode_append_command(graph, key, chunk_index, chunk)
        })?;
        let chunk = edge_chunk(
            &artifact.edges[offset..offset + candidate_count],
            &id_by_source,
        )?;
        let response = transport
            .command(BulkLoadCommand::Append {
                graph_name: graph.map(str::to_owned),
                client_bulk_key: key.to_owned(),
                chunk_index,
                chunk,
            })
            .map_err(LoadError::Remote)?;
        let next_offset = match response {
            BulkLoadResponse::Appended { next_offset, .. } => next_offset,
            other => {
                return Err(LoadError::Remote(format!(
                    "unexpected Append response: {other:?}"
                )));
            }
        };
        if next_offset == 0 {
            return Err(LoadError::Remote(
                "bulk-load Append committed zero operations".into(),
            ));
        }
        offset += next_offset as usize;
        chunk_index += 1;
        hint = Some(SizeHint::new(candidate_count));
    }

    let finalized = transport
        .command(BulkLoadCommand::Finalize {
            graph_name: graph.map(str::to_owned),
            client_bulk_key: key.to_owned(),
        })
        .map_err(LoadError::Remote)?;
    match finalized {
        BulkLoadResponse::FinalizeAccepted { .. } => {}
        other => {
            return Err(LoadError::Remote(format!(
                "unexpected Finalize response: {other:?}"
            )));
        }
    }

    for _ in 0..MAX_FINALIZE_POLL_ATTEMPTS {
        let (page, _) = status_paged(transport, graph, key)?
            .ok_or_else(|| LoadError::Remote("bulk-load job disappeared during finalize".into()))?;
        match page.state {
            BulkLoadPublicStateV1::Completed => {
                if let Some(path) = state_file {
                    write_state_file(path, digest, key, graph)?;
                }
                return Ok(LoadOutcome::Loaded {
                    key: key.to_owned(),
                });
            }
            BulkLoadPublicStateV1::Failed { reason } => {
                return Err(LoadError::Operator(format!("bulk load failed: {reason}")));
            }
            BulkLoadPublicStateV1::Aborted => {
                return Err(LoadError::Operator("bulk load was aborted".into()));
            }
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
    Err(LoadError::Operator(format!(
        "bulk load finalize did not complete after {MAX_FINALIZE_POLL_ATTEMPTS} polls; re-run the command to resume"
    )))
}

/// Fit the next candidate batch to the inter-canister payload bound using measured encoded sizes.
fn fit_candidate(
    remaining: usize,
    hint: Option<SizeHint>,
    measure: impl FnMut(usize) -> Result<usize, LoadError>,
) -> Result<usize, LoadError> {
    let fitted = adaptive_fitting_prefix(remaining, hint, SizingPolicy::inter_canister(), measure)
        .map_err(|error| match error {
            FitError::Measure(error) => error,
            FitError::NoEntryFits { .. } => {
                LoadError::Artifact("an entry does not fit the bulk-load payload bound".into())
            }
        })?
        .expect("non-empty candidate window");
    Ok(fitted.entry_count)
}

fn encode_append_command(
    graph: Option<&str>,
    key: &str,
    chunk_index: u32,
    chunk: BulkLoadChunkV1,
) -> Result<usize, LoadError> {
    let command = BulkLoadCommand::Append {
        graph_name: graph.map(str::to_owned),
        client_bulk_key: key.to_owned(),
        chunk_index,
        chunk,
    };
    Encode!(&command)
        .map(|bytes| bytes.len())
        .map_err(|error| LoadError::Artifact(format!("encode Append candidate: {error}")))
}

fn encode_value(value: &Value) -> Result<Vec<u8>, LoadError> {
    value
        .to_binary_bytes()
        .map_err(|error| LoadError::Artifact(format!("property value encode: {error}")))
}

fn encode_properties(properties: &Properties) -> Result<Vec<AtomicInsertPropertyV1>, LoadError> {
    properties
        .0
        .iter()
        .map(|(name, value)| {
            Ok(AtomicInsertPropertyV1 {
                property_name: name.clone(),
                value: encode_value(value)?,
            })
        })
        .collect()
}

fn vertex_chunk(rows: &[VertexRow]) -> Result<BulkLoadChunkV1, LoadError> {
    let items = rows
        .iter()
        .map(|row| {
            Ok(AtomicInsertVertexV1 {
                vertex_labels: row.labels.clone(),
                initial_properties: encode_properties(&row.properties)?,
            })
        })
        .collect::<Result<Vec<_>, LoadError>>()?;
    Ok(BulkLoadChunkV1::Vertices(items))
}

fn edge_chunk(
    rows: &[EdgeRow],
    id_by_source: &HashMap<String, Vec<u8>>,
) -> Result<BulkLoadChunkV1, LoadError> {
    let items = rows
        .iter()
        .map(|row| {
            let source = id_by_source.get(&row.source).ok_or_else(|| {
                LoadError::Artifact(format!("edge source {:?} lacks an encoded id", row.source))
            })?;
            let target = id_by_source.get(&row.target).ok_or_else(|| {
                LoadError::Artifact(format!("edge target {:?} lacks an encoded id", row.target))
            })?;
            Ok(BulkLoadEdgeV1 {
                source: source.clone(),
                target: target.clone(),
                directed: row.directed,
                edge_label_name: Some(row.label.clone()),
                inline_property: row.inline_value.as_ref().map(encode_value).transpose()?,
                initial_edge_properties: encode_properties(&row.properties)?,
            })
        })
        .collect::<Result<Vec<_>, LoadError>>()?;
    Ok(BulkLoadChunkV1::Edges(items))
}

// ──── entry point ────

/// Resolve the effective job key: the state-file record (resume/skip identity), a fresh derived
/// key under `--fresh`, or the base `--key`.
fn effective_key(args: &LoadArgs) -> Result<String, LoadError> {
    if args.fresh {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        return Ok(format!("{}.{nonce}", args.key));
    }
    if let Some(path) = &args.state_file
        && let Some(state) = read_state_file(path)?
    {
        return Ok(state.bulk_key);
    }
    Ok(args.key.clone())
}

/// Validate and run one `gleaph load` invocation.
pub fn execute(args: &LoadArgs) -> Result<LoadOutcome, LoadError> {
    let format = resolve_format(args)?;
    let artifact = read_artifact(&args.artifacts, format)?;
    validate_artifact(&artifact, args.graph.as_deref(), &args.key)?;
    let digest = artifact_digest(&args.artifacts, format)?;
    let key = effective_key(args)?;
    let mut transport = RemoteBulkLoadTransport::connect(
        &args.canister,
        &args.network,
        args.identity.as_deref(),
        args.fetch_root_key,
    )?;
    let outcome = run_load(
        &mut transport,
        &artifact,
        args.graph.as_deref(),
        &key,
        &digest,
        args.state_file.as_deref(),
    )?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_router::types::AtomicInsertReceiptV1;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_path(tag: &str) -> PathBuf {
        let nonce = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gleaph-cli-load-{}-{nonce}-{tag}",
            std::process::id()
        ))
    }

    fn write_temp(path: &Path, content: &str) {
        fs::write(path, content).expect("temporary file write");
    }

    fn load_args(artifacts: &[&str]) -> LoadArgs {
        LoadArgs {
            artifacts: artifacts.iter().map(PathBuf::from).collect(),
            canister: "aaaaa-aa".into(),
            graph: None,
            key: "test-key".into(),
            network: "local".into(),
            identity: None,
            fetch_root_key: false,
            format: None,
            fresh: false,
            state_file: None,
        }
    }

    fn vertex(source_id: &str, labels: &[&str]) -> VertexRow {
        VertexRow {
            source_id: source_id.into(),
            labels: labels.iter().map(|label| (*label).to_owned()).collect(),
            properties: Properties::default(),
        }
    }

    fn edge(source: &str, target: &str, label: &str) -> EdgeRow {
        EdgeRow {
            source: source.into(),
            target: target.into(),
            label: label.into(),
            directed: true,
            inline_value: None,
            properties: Properties::default(),
        }
    }

    #[test]
    fn validates_unique_source_ids_and_endpoint_resolution() {
        let artifact = LoadArtifact {
            vertices: vec![vertex("a", &["Person"]), vertex("a", &["Person"])],
            edges: Vec::new(),
        };
        let error = validate_artifact(&artifact, None, "k")
            .expect_err("duplicate source_id must be rejected");
        assert!(error.to_string().contains("duplicate vertex source_id"));

        let artifact = LoadArtifact {
            vertices: vec![vertex("a", &["Person"])],
            edges: vec![edge("a", "missing", "KNOWS")],
        };
        let error = validate_artifact(&artifact, None, "k")
            .expect_err("unresolved endpoint must be rejected");
        assert!(error.to_string().contains("does not resolve"));

        let artifact = LoadArtifact {
            vertices: vec![vertex("a", &[])],
            edges: Vec::new(),
        };
        let error =
            validate_artifact(&artifact, None, "k").expect_err("empty labels must be rejected");
        assert!(error.to_string().contains("non-empty labels"));
    }

    #[test]
    fn parses_json_artifact_with_canonical_values() {
        let path = temp_path("artifact.json");
        write_temp(
            &path,
            r#"{
                "format_version": 1,
                "vertices": [
                    {"source_id": "v1", "labels": ["Person"], "properties": {
                        "name": {"Text": "Alice"},
                        "joined": {"DateTime": {"seconds": 1700000000, "nanos": 5}}
                    }}
                ],
                "edges": [
                    {"source": "v1", "target": "v1", "label": "KNOWS", "directed": false}
                ]
            }"#,
        );
        let artifact =
            read_artifact(std::slice::from_ref(&path), Format::Json).expect("parse JSON artifact");
        assert_eq!(artifact.vertices.len(), 1);
        assert_eq!(artifact.vertices[0].properties.0.len(), 2);
        assert_eq!(
            artifact.vertices[0].properties.0[1].1,
            Value::DateTime(1_700_000_000, 5)
        );
        assert!(!artifact.edges[0].directed);
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn parses_yaml_artifact() {
        let path = temp_path("artifact.yaml");
        write_temp(
            &path,
            "format_version: 1\nvertices:\n  - source_id: v1\n    labels: [Person]\n    properties:\n      age: {Int64: 30}\n",
        );
        let artifact =
            read_artifact(std::slice::from_ref(&path), Format::Yaml).expect("parse YAML artifact");
        assert_eq!(artifact.vertices[0].properties.0[0].1, Value::Int64(30));
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn parses_jsonl_artifacts() {
        let vertices = temp_path("vertices.jsonl");
        let edges = temp_path("edges.jsonl");
        write_temp(
            &vertices,
            "{\"source_id\":\"v1\",\"labels\":[\"Person\"]}\n\n{\"source_id\":\"v2\",\"labels\":[\"Person\"]}\n",
        );
        write_temp(
            &edges,
            "{\"source\":\"v1\",\"target\":\"v2\",\"label\":\"KNOWS\"}\n",
        );
        let artifact = read_artifact(&[vertices.clone(), edges.clone()], Format::Jsonl)
            .expect("parse NDJSON artifacts");
        assert_eq!(artifact.vertices.len(), 2);
        assert_eq!(artifact.edges.len(), 1);
        fs::remove_file(vertices).expect("cleanup");
        fs::remove_file(edges).expect("cleanup");
    }

    #[test]
    fn rejects_unknown_format_version_and_duplicate_properties() {
        let path = temp_path("bad-version.json");
        write_temp(&path, r#"{"format_version": 2, "vertices": []}"#);
        let error = read_artifact(std::slice::from_ref(&path), Format::Json)
            .err()
            .expect("unknown format_version must be rejected");
        assert!(error.to_string().contains("format_version"));
        fs::remove_file(path).expect("cleanup");

        let path = temp_path("dup-props.json");
        write_temp(
            &path,
            r#"{"format_version": 1, "vertices": [{"source_id": "v1", "labels": ["P"], "properties": {"a": {"Int64": 1}, "a": {"Int64": 2}}}]}"#,
        );
        let error = read_artifact(std::slice::from_ref(&path), Format::Json)
            .err()
            .expect("duplicate property names must be rejected");
        assert!(error.to_string().contains("duplicate property"));
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn fresh_key_is_derived_and_state_file_reuses_the_recorded_key() {
        let mut args = load_args(&[]);
        args.fresh = true;
        args.key = "base".into();
        let fresh = effective_key(&args).expect("fresh key");
        assert!(fresh.starts_with("base."), "fresh key must derive: {fresh}");

        let state = temp_path("state.json");
        write_temp(
            &state,
            r#"{"format_version": 1, "artifact_sha256": "abc", "bulk_key": "recorded-key", "graph": null}"#,
        );
        let mut args = load_args(&[]);
        args.state_file = Some(state.clone());
        args.key = "base".into();
        let reused = effective_key(&args).expect("recorded key");
        assert_eq!(reused, "recorded-key");
        fs::remove_file(state).expect("cleanup");
    }

    #[test]
    fn exit_codes_follow_the_documented_contract() {
        assert_eq!(LoadError::Usage("x".into()).exit_code(), 2);
        assert_eq!(LoadError::Artifact("x".into()).exit_code(), 2);
        assert_eq!(LoadError::Remote("x".into()).exit_code(), 3);
        assert_eq!(LoadError::Operator("x".into()).exit_code(), 1);
    }

    // ──── loader with a fake durable transport ────

    struct FakeJob {
        next_chunk_index: u32,
        receipts: Vec<BulkLoadChunkReceiptV1>,
        state: BulkLoadPublicStateV1,
        next_vertex_ordinal: u64,
    }

    struct FakeBulkLoadTransport {
        job: Option<FakeJob>,
        /// Maximum operations committed by one Append (simulates the instruction budget).
        budget: usize,
    }

    impl BulkLoadTransport for FakeBulkLoadTransport {
        fn status(
            &mut self,
            _graph: Option<&str>,
            key: &str,
            cursor: Option<u32>,
            max_receipts: u32,
        ) -> Result<Result<BulkLoadStatusPage, RouterError>, String> {
            let Some(job) = &mut self.job else {
                return Ok(Err(RouterError::NotFound(key.into())));
            };
            if job.state == BulkLoadPublicStateV1::FinalizePending {
                job.state = BulkLoadPublicStateV1::Completed;
            }
            let cursor = cursor.unwrap_or(0) as usize;
            let page_receipts = job
                .receipts
                .iter()
                .skip(cursor)
                .take(max_receipts as usize)
                .cloned()
                .collect::<Vec<_>>();
            let next_cursor = (cursor + page_receipts.len() < job.receipts.len())
                .then_some((cursor + page_receipts.len()) as u32);
            Ok(Ok(BulkLoadStatusPage {
                state: job.state.clone(),
                next_chunk_index: job.next_chunk_index,
                committed_chunk_count: job.receipts.len() as u32,
                completed_chunk_count: job.receipts.len() as u32,
                terminal_at_ns: None,
                expires_at_ns: None,
                receipts: page_receipts,
                next_receipt_cursor: next_cursor,
            }))
        }

        fn command(&mut self, command: BulkLoadCommand) -> Result<BulkLoadResponse, String> {
            match command {
                BulkLoadCommand::Start { .. } => match &self.job {
                    Some(job) => Ok(BulkLoadResponse::Started {
                        next_chunk_index: job.next_chunk_index,
                    }),
                    None => {
                        self.job = Some(FakeJob {
                            next_chunk_index: 0,
                            receipts: Vec::new(),
                            state: BulkLoadPublicStateV1::Open,
                            next_vertex_ordinal: 0,
                        });
                        Ok(BulkLoadResponse::Started {
                            next_chunk_index: 0,
                        })
                    }
                },
                BulkLoadCommand::Append {
                    chunk_index, chunk, ..
                } => {
                    let job = self.job.as_mut().ok_or("append without start")?;
                    if chunk_index != job.next_chunk_index {
                        return Err(format!(
                            "chunk_index {chunk_index} != next {}",
                            job.next_chunk_index
                        ));
                    }
                    let total = match &chunk {
                        BulkLoadChunkV1::Vertices(items) => items.len(),
                        BulkLoadChunkV1::Edges(items) => items.len(),
                    };
                    let commit = total.min(self.budget);
                    let (vertex_count, edge_count) = match &chunk {
                        BulkLoadChunkV1::Vertices(_) => (commit as u64, 0),
                        BulkLoadChunkV1::Edges(_) => (0, commit as u64),
                    };
                    let mut ids = Vec::new();
                    if matches!(chunk, BulkLoadChunkV1::Vertices(_)) {
                        for _ in 0..commit {
                            let id = job.next_vertex_ordinal;
                            job.next_vertex_ordinal += 1;
                            ids.push(id.to_le_bytes().to_vec());
                        }
                    }
                    let receipt = AtomicInsertReceiptV1 {
                        logical_operation_count: commit as u64,
                        logical_vertex_count: vertex_count,
                        logical_edge_count: edge_count,
                        allocated_vertex_ids: ids,
                    };
                    job.receipts.push(BulkLoadChunkReceiptV1 {
                        chunk_index,
                        receipt: receipt.clone(),
                    });
                    job.next_chunk_index += 1;
                    job.state = BulkLoadPublicStateV1::AppendPending;
                    Ok(BulkLoadResponse::Appended {
                        chunk_index,
                        next_offset: commit as u32,
                        receipt,
                    })
                }
                BulkLoadCommand::Finalize { .. } => {
                    let job = self.job.as_mut().ok_or("finalize without start")?;
                    job.state = BulkLoadPublicStateV1::FinalizePending;
                    Ok(BulkLoadResponse::FinalizeAccepted {
                        state: job.state.clone(),
                    })
                }
                BulkLoadCommand::Abort { .. } => {
                    let job = self.job.as_mut().ok_or("abort without start")?;
                    job.state = BulkLoadPublicStateV1::AbortPending;
                    Ok(BulkLoadResponse::AbortAccepted {
                        state: job.state.clone(),
                    })
                }
            }
        }
    }

    fn sample_artifact(vertices: usize, edges: usize) -> LoadArtifact {
        LoadArtifact {
            vertices: (0..vertices)
                .map(|index| vertex(&format!("v{index}"), &["Person"]))
                .collect(),
            edges: (0..edges)
                .map(|index| edge("v0", &format!("v{}", index + 1), "KNOWS"))
                .collect(),
        }
    }

    #[test]
    fn loader_loads_vertices_then_edges_and_loops_on_next_offset() {
        let artifact = sample_artifact(5, 2);
        let mut transport = FakeBulkLoadTransport {
            job: None,
            budget: 3, // forces multiple Append calls per phase
        };
        let outcome = run_load(&mut transport, &artifact, None, "k", "digest", None)
            .expect("load should complete");
        assert_eq!(outcome, LoadOutcome::Loaded { key: "k".into() });
        let job = transport.job.expect("job exists");
        let vertex_chunks = job
            .receipts
            .iter()
            .filter(|row| row.receipt.logical_vertex_count > 0)
            .count();
        assert_eq!(vertex_chunks, 2, "5 vertices at budget 3 need 2 chunks");
        let edge_chunks = job
            .receipts
            .iter()
            .filter(|row| row.receipt.logical_edge_count > 0)
            .count();
        assert_eq!(edge_chunks, 1);
        assert!(matches!(job.state, BulkLoadPublicStateV1::Completed));
    }

    #[test]
    fn loader_resumes_from_receipts_without_reloading_committed_prefix() {
        let artifact = sample_artifact(5, 2);
        let mut transport = FakeBulkLoadTransport {
            job: Some(FakeJob {
                next_chunk_index: 2,
                receipts: vec![
                    BulkLoadChunkReceiptV1 {
                        chunk_index: 0,
                        receipt: AtomicInsertReceiptV1 {
                            logical_operation_count: 2,
                            logical_vertex_count: 2,
                            logical_edge_count: 0,
                            allocated_vertex_ids: vec![vec![0], vec![1]],
                        },
                    },
                    BulkLoadChunkReceiptV1 {
                        chunk_index: 1,
                        receipt: AtomicInsertReceiptV1 {
                            logical_operation_count: 2,
                            logical_vertex_count: 2,
                            logical_edge_count: 0,
                            allocated_vertex_ids: vec![vec![2], vec![3]],
                        },
                    },
                ],
                state: BulkLoadPublicStateV1::AppendPending,
                next_vertex_ordinal: 4,
            }),
            budget: usize::MAX,
        };
        let outcome = run_load(&mut transport, &artifact, None, "k", "digest", None)
            .expect("resume should complete");
        assert_eq!(outcome, LoadOutcome::Loaded { key: "k".into() });
        let job = transport.job.expect("job exists");
        // The vertex phase resumes at 4 committed vertices: one more vertex chunk.
        let vertex_chunks = job
            .receipts
            .iter()
            .filter(|row| row.receipt.logical_vertex_count > 0)
            .count();
        assert_eq!(vertex_chunks, 3);
        // The edge chunk endpoints resolve to ids allocated across the resume and this run.
        assert!(matches!(job.state, BulkLoadPublicStateV1::Completed));
    }

    #[test]
    fn loader_skips_a_completed_job_with_matching_digest() {
        let artifact = sample_artifact(1, 0);
        let mut transport = FakeBulkLoadTransport {
            job: Some(FakeJob {
                next_chunk_index: 1,
                receipts: Vec::new(),
                state: BulkLoadPublicStateV1::Completed,
                next_vertex_ordinal: 1,
            }),
            budget: usize::MAX,
        };
        let state = temp_path("state.json");
        write_temp(
            &state,
            r#"{"format_version": 1, "artifact_sha256": "digest", "bulk_key": "k", "graph": null}"#,
        );
        let outcome = run_load(&mut transport, &artifact, None, "k", "digest", Some(&state))
            .expect("matching completed job must skip");
        assert_eq!(outcome, LoadOutcome::Skipped { key: "k".into() });
        fs::remove_file(state).expect("cleanup");
    }

    #[test]
    fn loader_rejects_completed_job_when_the_artifact_changed() {
        let artifact = sample_artifact(1, 0);
        let mut transport = FakeBulkLoadTransport {
            job: Some(FakeJob {
                next_chunk_index: 1,
                receipts: Vec::new(),
                state: BulkLoadPublicStateV1::Completed,
                next_vertex_ordinal: 1,
            }),
            budget: usize::MAX,
        };
        let state = temp_path("state.json");
        write_temp(
            &state,
            r#"{"format_version": 1, "artifact_sha256": "old-digest", "bulk_key": "k", "graph": null}"#,
        );
        let error = run_load(
            &mut transport,
            &artifact,
            None,
            "k",
            "new-digest",
            Some(&state),
        )
        .expect_err("changed artifact must not be silently skipped");
        assert_eq!(error.exit_code(), 1);
        fs::remove_file(state).expect("cleanup");
    }

    #[test]
    fn loader_rejects_a_terminal_failed_job() {
        let artifact = sample_artifact(1, 0);
        let mut transport = FakeBulkLoadTransport {
            job: Some(FakeJob {
                next_chunk_index: 3,
                receipts: Vec::new(),
                state: BulkLoadPublicStateV1::Failed {
                    reason: "boom".into(),
                },
                next_vertex_ordinal: 3,
            }),
            budget: usize::MAX,
        };
        let error = run_load(&mut transport, &artifact, None, "k", "digest", None)
            .expect_err("terminal failed job must be reported");
        assert!(error.to_string().contains("use a new --key"));
        assert_eq!(error.exit_code(), 1);
    }
}
