//! `gleaph load`: load initial vertices and edges into an existing logical graph through the
//! durable Router `bulk_load` lifecycle (ADR 0057, ADR 0060 Decision 4).
//!
//! The artifact is a YAML/JSON single file (`format_version: 1`, `vertices` + `edges`) or two
//! NDJSON files (`vertices.jsonl` + `edges.jsonl`). NDJSON files are read as a row stream: a
//! first pass validates every row and computes the artifact digest without materializing rows,
//! and a second pass re-reads the files to build budget-fitted chunks, so peak memory stays
//! bounded by one chunk plus the compact `source_id`/vertex-id index rather than the file size.
//! The CLI validates everything before any remote call, then drives the lifecycle: `Start` →
//! vertex chunks → edge chunks → `Finalize` → poll `Completed`. An edge endpoint is either an
//! in-artifact `source_id` reference (resolved to the encoded vertex IDs allocated by the Router)
//! or a `{ label, property, value }` reference that the Router resolves through the graph
//! property index. Chunk boundaries are Router-owned (`Resumable` execution commits a
//! budget-fitting prefix and returns `next_offset`); the CLI only fits each request to the
//! ingress payload bound and loops.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, IsTerminal};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use candid::Encode;
use clap::Args;
use gleaph_bulk_load_api::{
    AtomicInsertPropertyV1, AtomicInsertVertexV1, BulkLoadChunkReceiptV1, BulkLoadChunkV1,
    BulkLoadCommand, BulkLoadEdgeV1, BulkLoadEndpointV1, BulkLoadPropertyEndpointV1,
    BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
};
use gleaph_gql::value::Value;
use gleaph_gql::value_to_index_key_bytes;
use gleaph_graph_kernel::federation::RouterError;
use gleaph_message_sizing::{FitError, SizeHint, SizingPolicy, adaptive_fitting_prefix};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::progress::ProgressLine;
use crate::remote::RemoteTransport;

const FORMAT_VERSION: u32 = 1;
/// Bounded single-file input. Larger artifacts must use NDJSON, which is the streaming family.
/// The cap also bounds YAML alias-expansion work for untrusted artifacts.
const MAX_SINGLE_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// NDJSON streaming: rows accumulated before the first chunk fit when no size hint exists yet.
const INITIAL_CHUNK_ROWS: usize = 4096;
/// NDJSON streaming: raw-line bytes bound for one candidate chunk window. Keeps peak memory
/// independent of property-value size when individual rows are large.
const MAX_ACCUMULATED_RAW_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const DEFAULT_BULK_KEY: &str = "initial-load-v1";
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
    pub artifacts: Vec<PathBuf>,
    /// Router canister principal (required unless supplied by GLEAPH_CANISTER or `gleaph.toml`).
    #[arg(long, value_name = "PRINCIPAL")]
    pub canister: Option<String>,
    /// Logical graph name; omitted for the caller's default (HOME) graph.
    #[arg(long, value_name = "NAME")]
    pub graph: Option<String>,
    /// Durable bulk-load job key. Terminal jobs are single-use: a completed/failed/aborted key
    /// cannot be reused, so re-loading after a terminal state requires a new key or `--fresh`.
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
    /// Artifact format; inferred from the file extension when omitted.
    #[arg(long, value_name = "FORMAT")]
    pub format: Option<Format>,
    /// NDJSON vertices file only (no edges). Mutually exclusive with positional ARTIFACT.
    #[arg(long, value_name = "FILE")]
    pub vertices: Option<PathBuf>,
    /// NDJSON edges file only (no vertices). Endpoints must use the property-based
    /// `{ label, property, value }` form; `source_id` references cannot resolve without vertices
    /// in the same artifact.
    #[arg(long, value_name = "FILE")]
    pub edges: Option<PathBuf>,
    /// Start a new job under a derived key instead of resuming or skipping; the effective key is
    /// printed and recorded in `--state-file` when given.
    #[arg(long)]
    pub fresh: bool,
    /// State file recording the effective job key and the loaded artifact digest. When present,
    /// the recorded key is reused (resume/skip identity) and skip-on-Completed requires the
    /// digest to match.
    #[arg(long, value_name = "PATH")]
    pub state_file: Option<PathBuf>,
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

#[derive(Debug)]
struct LoadArtifact {
    vertices: Vec<VertexRow>,
    edges: Vec<EdgeRow>,
}

/// Order-preserving property map with duplicate-name rejection (the wire requires unique names).
#[derive(Clone, Debug, Default)]
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

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VertexRow {
    source_id: String,
    labels: Vec<String>,
    #[serde(default)]
    properties: Properties,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EdgeRow {
    /// Source vertex: an in-artifact `source_id` reference, or a property-based endpoint.
    source: EdgeEndpointRow,
    /// Target vertex: an in-artifact `source_id` reference, or a property-based endpoint.
    target: EdgeEndpointRow,
    label: String,
    #[serde(default = "default_directed")]
    directed: bool,
    #[serde(default)]
    inline_value: Option<Value>,
    #[serde(default)]
    properties: Properties,
}

/// One edge endpoint in an artifact row. A bare string is an in-artifact `source_id` reference
/// resolved against the vertices loaded in the same artifact; an object references an existing
/// vertex by `{ label, property, value }` and is resolved by the Router through the graph
/// property index.
#[derive(Deserialize, Clone, Debug)]
#[serde(untagged)]
enum EdgeEndpointRow {
    SourceId(String),
    ByProperty(PropertyEndpointRow),
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
struct PropertyEndpointRow {
    label: String,
    property: String,
    value: Value,
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

/// A validated artifact ready for the loader: either bounded in-memory rows (YAML/JSON single
/// file) or validated NDJSON streams that are re-read during dispatch. `digest` is the sha256 of
/// the raw artifact bytes (state-file identity).
#[derive(Debug)]
enum PreparedLoad {
    SingleFile {
        artifact: LoadArtifact,
        digest: String,
        property_names: BTreeSet<String>,
    },
    Njson {
        vertices: Option<PathBuf>,
        edges: Option<PathBuf>,
        digest: String,
        vertex_count: usize,
        edge_count: usize,
        property_names: BTreeSet<String>,
    },
}

impl PreparedLoad {
    fn digest(&self) -> &str {
        match self {
            PreparedLoad::SingleFile { digest, .. } | PreparedLoad::Njson { digest, .. } => digest,
        }
    }

    fn vertex_count(&self) -> usize {
        match self {
            PreparedLoad::SingleFile { artifact, .. } => artifact.vertices.len(),
            PreparedLoad::Njson { vertex_count, .. } => *vertex_count,
        }
    }

    fn edge_count(&self) -> usize {
        match self {
            PreparedLoad::SingleFile { artifact, .. } => artifact.edges.len(),
            PreparedLoad::Njson { edge_count, .. } => *edge_count,
        }
    }

    /// Distinct property names referenced by any vertex or edge row. Data-driven properties are
    /// interned before the first chunk (ADR 0059: the Router rejects missing properties).
    fn property_names(&self) -> &BTreeSet<String> {
        match self {
            PreparedLoad::SingleFile { property_names, .. }
            | PreparedLoad::Njson { property_names, .. } => property_names,
        }
    }

    /// Row source for the vertex phase, starting at the first row of the artifact. Rows committed
    /// by a prior run are matched to their recorded ids by the phase, not skipped.
    fn vertex_stream(&self) -> Result<RowStream<'_, VertexRow>, LoadError> {
        match self {
            PreparedLoad::SingleFile { artifact, .. } => {
                Ok(RowStream::Memory(artifact.vertices.iter()))
            }
            PreparedLoad::Njson {
                vertices: Some(path),
                ..
            } => Ok(RowStream::File(NjsonRowReader::new(path)?)),
            PreparedLoad::Njson { vertices: None, .. } => Ok(RowStream::Empty),
        }
    }

    /// Row source for the edge phase, starting at the first row of the artifact; the phase skips
    /// rows committed by a prior run.
    fn edge_stream(&self) -> Result<RowStream<'_, EdgeRow>, LoadError> {
        match self {
            PreparedLoad::SingleFile { artifact, .. } => {
                Ok(RowStream::Memory(artifact.edges.iter()))
            }
            PreparedLoad::Njson {
                edges: Some(path), ..
            } => Ok(RowStream::File(NjsonRowReader::new(path)?)),
            PreparedLoad::Njson { edges: None, .. } => Ok(RowStream::Empty),
        }
    }
}

// ──── format detection and reading ────

/// Resolved artifact input: one YAML/JSON file, or NDJSON files designated by positionals or
/// `--vertices` / `--edges`.
#[derive(Debug)]
enum ArtifactInput {
    /// One YAML/JSON file containing `vertices` and `edges`.
    SingleFile { path: PathBuf, format: Format },
    /// NDJSON vertices and edges files.
    Both { vertices: PathBuf, edges: PathBuf },
    /// NDJSON vertices file only.
    VerticesOnly { path: PathBuf },
    /// NDJSON edges file only.
    EdgesOnly { path: PathBuf },
}

fn resolve_input(args: &LoadArgs) -> Result<ArtifactInput, LoadError> {
    if args.vertices.is_some() || args.edges.is_some() {
        if !args.artifacts.is_empty() {
            return Err(LoadError::Usage(
                "cannot combine positional ARTIFACT with --vertices/--edges".into(),
            ));
        }
        if let Some(format) = args.format
            && format != Format::Jsonl
        {
            return Err(LoadError::Usage(format!(
                "--format {format:?} does not apply to --vertices/--edges (NDJSON)"
            )));
        }
        let require_jsonl = |path: &Path, flag: &str| -> Result<PathBuf, LoadError> {
            let ext = extension(path)?;
            if !matches!(ext.as_str(), "jsonl" | "ndjson") {
                return Err(LoadError::Usage(format!(
                    "{flag} requires an NDJSON (.jsonl) file; got extension {ext:?}"
                )));
            }
            Ok(path.to_owned())
        };
        return match (&args.vertices, &args.edges) {
            (Some(vertices), Some(edges)) => Ok(ArtifactInput::Both {
                vertices: require_jsonl(vertices, "--vertices")?,
                edges: require_jsonl(edges, "--edges")?,
            }),
            (Some(vertices), None) => Ok(ArtifactInput::VerticesOnly {
                path: require_jsonl(vertices, "--vertices")?,
            }),
            (None, Some(edges)) => Ok(ArtifactInput::EdgesOnly {
                path: require_jsonl(edges, "--edges")?,
            }),
            (None, None) => unreachable!("at least one of --vertices/--edges is present"),
        };
    }
    match args.artifacts.len() {
        1 => {
            let path = &args.artifacts[0];
            let format = match args.format {
                Some(Format::Jsonl) => {
                    return Err(LoadError::Usage(
                        "a single NDJSON file is ambiguous; use --vertices or --edges".into(),
                    ));
                }
                Some(format @ (Format::Yaml | Format::Json)) => format,
                None => match extension(path)?.as_str() {
                    "json" => Format::Json,
                    "yaml" | "yml" => Format::Yaml,
                    "jsonl" | "ndjson" => {
                        return Err(LoadError::Usage(
                            "a single NDJSON file is ambiguous; use --vertices or --edges".into(),
                        ));
                    }
                    other => {
                        return Err(LoadError::Usage(format!(
                            "cannot infer artifact format from extension {other:?}; pass --format yaml|json|jsonl"
                        )));
                    }
                },
            };
            Ok(ArtifactInput::SingleFile {
                path: path.clone(),
                format,
            })
        }
        2 => {
            for path in &args.artifacts {
                let ext = extension(path)?;
                if !matches!(ext.as_str(), "jsonl" | "ndjson") {
                    return Err(LoadError::Usage(format!(
                        "two artifact files require NDJSON (.jsonl); got extension {ext:?}"
                    )));
                }
            }
            Ok(ArtifactInput::Both {
                vertices: args.artifacts[0].clone(),
                edges: args.artifacts[1].clone(),
            })
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

fn read_single_file(path: &Path, format: Format) -> Result<(LoadArtifact, String), LoadError> {
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
    let digest = digest_bytes(&bytes);
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
    Ok((
        LoadArtifact {
            vertices: parsed.vertices,
            edges: parsed.edges,
        },
        digest,
    ))
}

fn digest_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(&hasher.finalize())
}

/// One row at a time from either the bounded single-file rows or an NDJSON file stream. The
/// memory variant is used by the single-file path (rows already validated and capped at
/// `MAX_SINGLE_FILE_BYTES`); the file variant owns the reader and re-parses on demand.
enum RowStream<'a, T> {
    Empty,
    Memory(std::slice::Iter<'a, T>),
    File(NjsonRowReader<T>),
}

impl<T: for<'de> Deserialize<'de> + Clone> RowStream<'_, T> {
    /// Next row plus the raw source bytes it occupied (0 for the memory variant).
    fn next_row_with_bytes(&mut self) -> Result<Option<(T, usize)>, LoadError> {
        match self {
            RowStream::Empty => Ok(None),
            RowStream::Memory(iter) => Ok(iter.next().cloned().map(|row| (row, 0))),
            RowStream::File(reader) => reader.next_row_with_bytes(),
        }
    }

    /// Discard `count` rows without parsing (rows committed by a prior run).
    fn skip(&mut self, count: usize) -> Result<(), LoadError> {
        match self {
            RowStream::Empty => Ok(()),
            RowStream::Memory(iter) => {
                for _ in 0..count {
                    if iter.next().is_none() {
                        break;
                    }
                }
                Ok(())
            }
            RowStream::File(reader) => reader.skip_rows(count),
        }
    }
}

/// NDJSON row reader owned by [`RowStream::File`]. `line_index` counts every line (including
/// blanks) so parse errors carry the same 1-based line numbers as the pre-scan.
struct NjsonRowReader<T> {
    path: PathBuf,
    reader: BufReader<fs::File>,
    line_index: usize,
    _marker: PhantomData<T>,
}

impl<T> NjsonRowReader<T> {
    fn new(path: &Path) -> Result<Self, LoadError> {
        let file = fs::File::open(path)
            .map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
        Ok(Self {
            path: path.to_owned(),
            reader: BufReader::new(file),
            line_index: 0,
            _marker: PhantomData,
        })
    }
}

impl<T: for<'de> Deserialize<'de>> NjsonRowReader<T> {
    fn next_row_with_bytes(&mut self) -> Result<Option<(T, usize)>, LoadError> {
        let mut raw = String::new();
        loop {
            raw.clear();
            let n = self
                .reader
                .read_line(&mut raw)
                .map_err(|error| LoadError::Artifact(format!("read {:?}: {error}", self.path)))?;
            if n == 0 {
                return Ok(None);
            }
            self.line_index += 1;
            if raw.trim().is_empty() {
                continue;
            }
            let row: T = serde_json::from_str(raw.trim()).map_err(|error| {
                LoadError::Artifact(format!(
                    "parse {:?} line {}: {error}",
                    self.path, self.line_index
                ))
            })?;
            return Ok(Some((row, n)));
        }
    }

    fn skip_rows(&mut self, count: usize) -> Result<(), LoadError> {
        let mut raw = String::new();
        let mut skipped = 0usize;
        while skipped < count {
            raw.clear();
            let n = self
                .reader
                .read_line(&mut raw)
                .map_err(|error| LoadError::Artifact(format!("read {:?}: {error}", self.path)))?;
            if n == 0 {
                break;
            }
            self.line_index += 1;
            if !raw.trim().is_empty() {
                skipped += 1;
            }
        }
        Ok(())
    }
}

/// Outcome of the NDJSON streaming pre-scan: validation results and the raw-file digest.
#[derive(Debug)]
struct NjsonScan {
    digest: String,
    vertex_count: usize,
    edge_count: usize,
    property_names: BTreeSet<String>,
}

/// Stream-validate every NDJSON row and hash the raw file bytes (the state-file digest). No row
/// is materialized; only the vertex `source_id` set is retained for cross-row checks.
fn scan_njson(vertices: Option<&Path>, edges: Option<&Path>) -> Result<NjsonScan, LoadError> {
    let mut hasher = Sha256::new();
    let mut source_ids = HashSet::new();
    let mut property_names = BTreeSet::new();
    let mut vertex_count = 0usize;
    if let Some(path) = vertices {
        for_each_njson_line(path, &mut hasher, |index, line| {
            let row: VertexRow = serde_json::from_str(line).map_err(|error| {
                LoadError::Artifact(format!("parse {path:?} line {}: {error}", index + 1))
            })?;
            for (name, _) in &row.properties.0 {
                property_names.insert(name.clone());
            }
            validate_vertex_row(vertex_count, &row, &mut source_ids)
        })?;
        vertex_count = source_ids.len();
    }
    let has_vertices = vertex_count > 0;
    let mut edge_count = 0usize;
    if let Some(path) = edges {
        for_each_njson_line(path, &mut hasher, |index, line| {
            let row: EdgeRow = serde_json::from_str(line).map_err(|error| {
                LoadError::Artifact(format!("parse {path:?} line {}: {error}", index + 1))
            })?;
            for (name, _) in &row.properties.0 {
                property_names.insert(name.clone());
            }
            validate_edge_row(edge_count, &row, has_vertices, &source_ids)?;
            edge_count += 1;
            Ok(())
        })?;
    }
    Ok(NjsonScan {
        digest: hex_digest(&hasher.finalize()),
        vertex_count,
        edge_count,
        property_names,
    })
}

fn for_each_njson_line(
    path: &Path,
    hasher: &mut Sha256,
    mut visit: impl FnMut(usize, &str) -> Result<(), LoadError>,
) -> Result<(), LoadError> {
    let file = fs::File::open(path)
        .map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
    let mut reader = BufReader::new(file);
    let mut raw = String::new();
    let mut line_index = 0usize;
    loop {
        raw.clear();
        let n = reader
            .read_line(&mut raw)
            .map_err(|error| LoadError::Artifact(format!("read {path:?}: {error}")))?;
        if n == 0 {
            return Ok(());
        }
        hasher.update(raw.as_bytes());
        let line = raw.trim();
        if !line.is_empty() {
            visit(line_index, line)?;
        }
        line_index += 1;
    }
}

/// Distinct property names across a loaded row set (vertex and edge properties).
fn collect_property_names(vertices: &[VertexRow], edges: &[EdgeRow]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (name, _) in vertices
        .iter()
        .flat_map(|row| row.properties.0.iter())
        .chain(edges.iter().flat_map(|row| row.properties.0.iter()))
    {
        names.insert(name.clone());
    }
    names
}

/// Read, validate, and digest one artifact without any remote call. NDJSON is stream-validated
/// (rows are not materialized); the YAML/JSON single file is bounded by `MAX_SINGLE_FILE_BYTES`.
fn prepare_artifact(
    input: &ArtifactInput,
    graph: Option<&str>,
    key: &str,
) -> Result<PreparedLoad, LoadError> {
    validate_load_options(graph, key)?;
    match input {
        ArtifactInput::SingleFile { path, format } => {
            let (artifact, digest) = read_single_file(path, *format)?;
            validate_artifact(&artifact)?;
            let property_names = collect_property_names(&artifact.vertices, &artifact.edges);
            Ok(PreparedLoad::SingleFile {
                artifact,
                digest,
                property_names,
            })
        }
        ArtifactInput::Both { vertices, edges } => {
            let scan = scan_njson(Some(vertices), Some(edges))?;
            Ok(PreparedLoad::Njson {
                vertices: Some(vertices.clone()),
                edges: Some(edges.clone()),
                digest: scan.digest,
                vertex_count: scan.vertex_count,
                edge_count: scan.edge_count,
                property_names: scan.property_names,
            })
        }
        ArtifactInput::VerticesOnly { path } => {
            let scan = scan_njson(Some(path), None)?;
            Ok(PreparedLoad::Njson {
                vertices: Some(path.clone()),
                edges: None,
                digest: scan.digest,
                vertex_count: scan.vertex_count,
                edge_count: scan.edge_count,
                property_names: scan.property_names,
            })
        }
        ArtifactInput::EdgesOnly { path } => {
            let scan = scan_njson(None, Some(path))?;
            Ok(PreparedLoad::Njson {
                vertices: None,
                edges: Some(path.clone()),
                digest: scan.digest,
                vertex_count: scan.vertex_count,
                edge_count: scan.edge_count,
                property_names: scan.property_names,
            })
        }
    }
}

// ──── validation ────

fn validate_load_options(graph: Option<&str>, key: &str) -> Result<(), LoadError> {
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
    Ok(())
}

/// Validate the bounded single-file artifact rows with the same per-row rules as the NDJSON
/// streaming pre-scan.
fn validate_artifact(artifact: &LoadArtifact) -> Result<(), LoadError> {
    let mut source_ids = HashSet::new();
    for (index, vertex) in artifact.vertices.iter().enumerate() {
        validate_vertex_row(index, vertex, &mut source_ids)?;
    }
    let has_vertices = !artifact.vertices.is_empty();
    for (index, edge) in artifact.edges.iter().enumerate() {
        validate_edge_row(index, edge, has_vertices, &source_ids)?;
    }
    Ok(())
}

fn validate_vertex_row(
    index: usize,
    vertex: &VertexRow,
    source_ids: &mut HashSet<String>,
) -> Result<(), LoadError> {
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
    validate_properties(index, &vertex.properties)
}

fn validate_edge_row(
    index: usize,
    edge: &EdgeRow,
    has_vertices: bool,
    source_ids: &HashSet<String>,
) -> Result<(), LoadError> {
    validate_edge_endpoint(index, "source", &edge.source, has_vertices, source_ids)?;
    validate_edge_endpoint(index, "target", &edge.target, has_vertices, source_ids)?;
    if edge.label.is_empty() {
        return Err(LoadError::Artifact(format!(
            "edges[{index}] label must not be empty"
        )));
    }
    validate_properties(index, &edge.properties)
}

/// Validate one edge endpoint. A `source_id` reference must resolve against the vertices loaded
/// in the same artifact; an edges-only artifact must use property-based endpoints exclusively.
/// A property-based endpoint must name a non-empty label/property and carry an indexable value.
fn validate_edge_endpoint(
    index: usize,
    name: &str,
    endpoint: &EdgeEndpointRow,
    has_vertices: bool,
    source_ids: &HashSet<String>,
) -> Result<(), LoadError> {
    match endpoint {
        EdgeEndpointRow::SourceId(source_id) => {
            if source_id.is_empty() {
                return Err(LoadError::Artifact(format!(
                    "edges[{index}] {name} source_id must not be empty"
                )));
            }
            if !has_vertices {
                return Err(LoadError::Artifact(format!(
                    "edges[{index}] {name} is a source_id reference but the artifact has no vertices; \
                     use a {{ label, property, value }} endpoint for edges-only loads"
                )));
            }
            if !source_ids.contains(source_id) {
                return Err(LoadError::Artifact(format!(
                    "edges[{index}] {name} does not resolve to a vertex source_id"
                )));
            }
            Ok(())
        }
        EdgeEndpointRow::ByProperty(property) => {
            if property.label.is_empty() {
                return Err(LoadError::Artifact(format!(
                    "edges[{index}] {name} label must not be empty"
                )));
            }
            if property.property.is_empty() {
                return Err(LoadError::Artifact(format!(
                    "edges[{index}] {name} property must not be empty"
                )));
            }
            if value_to_index_key_bytes(&property.value)
                .map_err(|error| {
                    LoadError::Artifact(format!("edges[{index}] {name} value encode: {error}"))
                })?
                .is_none()
            {
                return Err(LoadError::Artifact(format!(
                    "edges[{index}] {name} value is not a sortable (indexable) type"
                )));
            }
            Ok(())
        }
    }
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

    /// One `bulk_load` command. The outer error is a transport failure; the inner `Err` is the
    /// Router's typed rejection.
    fn command(
        &mut self,
        command: BulkLoadCommand,
    ) -> Result<Result<BulkLoadResponse, RouterError>, String>;

    /// Intern the data-driven property vocabulary before the first chunk. `bulk_load` admission
    /// resolves every property name against the Router catalog and rejects missing properties
    /// (ADR 0059), so the CLI declares the artifact's property names up front in one batch call.
    fn ensure_properties(
        &mut self,
        graph: &str,
        names: &[String],
    ) -> Result<Result<(), RouterError>, String>;
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
        self.remote.query_args::<BulkLoadStatusPage, RouterError>(
            "bulk_load_status",
            (
                &graph.map(str::to_owned),
                &key.to_owned(),
                &cursor,
                &max_receipts,
            ),
        )
    }

    fn command(
        &mut self,
        command: BulkLoadCommand,
    ) -> Result<Result<BulkLoadResponse, RouterError>, String> {
        self.remote
            .update::<BulkLoadResponse, RouterError>("bulk_load", &command)
    }

    fn ensure_properties(
        &mut self,
        graph: &str,
        names: &[String],
    ) -> Result<Result<(), RouterError>, String> {
        self.remote
            .update_args::<Vec<gleaph_graph_kernel::entry::PropertyId>, RouterError>(
                "ensure_properties",
                (&graph.to_string(), &names.to_vec()),
            )
            .map(|result| result.map(|_| ()))
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

/// Execute one `bulk_load` command and map a typed Router rejection to the documented exit
/// code: an `InvalidArgument` / `NotFound` rejection reflects graph state that needs an operator
/// action (missing property index, unresolved or non-unique property endpoints, unknown label or
/// property), while transport-level failures and other rejections stay remote/auth (exit 3).
fn send_command(
    transport: &mut impl BulkLoadTransport,
    command: BulkLoadCommand,
) -> Result<BulkLoadResponse, LoadError> {
    match transport.command(command).map_err(LoadError::Remote)? {
        Ok(response) => Ok(response),
        Err(RouterError::InvalidArgument(message)) | Err(RouterError::NotFound(message)) => Err(
            LoadError::Operator(format!("Router rejected bulk_load: {message}")),
        ),
        Err(error) => Err(LoadError::Remote(format!(
            "Router rejected bulk_load: {error:?}"
        ))),
    }
}

/// Drive one durable bulk-load job to `Completed` (ADR 0060 §3 client loop).
fn run_load(
    transport: &mut impl BulkLoadTransport,
    prepared: &PreparedLoad,
    graph: Option<&str>,
    key: &str,
    state_file: Option<&Path>,
) -> Result<LoadOutcome, LoadError> {
    let digest = prepared.digest();
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

    // The artifact's data-driven properties are interned in one batch call before the first chunk
    // (ADR 0059: bulk_load admission rejects missing properties). Interning is idempotent, so a
    // resumed job re-declares them safely. Without a graph name the CLI cannot declare properties,
    // so a property-carrying artifact fails fast with guidance instead of a cryptic Router
    // rejection mid-load.
    let property_names = prepared.property_names();
    if !property_names.is_empty() {
        let Some(graph_name) = graph else {
            return Err(LoadError::Operator(
                "the artifact carries properties that must be interned, but no graph name is \
                 available; pass --graph (or set [load] graph) before loading"
                    .into(),
            ));
        };
        let names: Vec<String> = property_names.iter().cloned().collect();
        match transport
            .ensure_properties(graph_name, &names)
            .map_err(LoadError::Remote)?
        {
            Ok(()) => {}
            Err(RouterError::InvalidArgument(message)) | Err(RouterError::NotFound(message)) => {
                return Err(LoadError::Operator(format!(
                    "Router rejected ensure_properties: {message}"
                )));
            }
            Err(error) => {
                return Err(LoadError::Remote(format!(
                    "Router rejected ensure_properties: {error:?}"
                )));
            }
        }
    }

    let started = send_command(
        transport,
        BulkLoadCommand::Start {
            graph_name: graph.map(str::to_owned),
            client_bulk_key: key.to_owned(),
        },
    )?;
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
    let Resume {
        committed_vertices,
        committed_edges,
        next_chunk_index,
        encoded_ids,
    } = resume_point(&page, &receipts);
    let mut chunk_index = next_chunk_index;
    let mut id_by_source: HashMap<String, Vec<u8>> =
        HashMap::with_capacity(prepared.vertex_count());

    // Vertex phase: stream rows into budget-fitted chunks; each Append commits a budget-fitting
    // prefix and the uncommitted tail stays buffered for the next chunk. Rows committed by a
    // prior run are matched to their recorded ids instead of being re-dispatched.
    let tty = std::io::stdout().is_terminal();
    let mut vertex_phase =
        PhaseProgress::new("vertices", prepared.vertex_count(), committed_vertices, tty);
    let mut vertex_stream = prepared.vertex_stream()?;
    run_vertex_phase(
        &mut vertex_stream,
        transport,
        graph,
        key,
        CommittedVertices {
            rows: committed_vertices,
            ids: encoded_ids,
        },
        &mut chunk_index,
        &mut id_by_source,
        &mut vertex_phase,
    )?;
    vertex_phase.finish();

    // Edge phase: stream edge rows and resolve each endpoint against the vertex ids allocated in
    // vertex order.
    let mut edge_phase = PhaseProgress::new("edges", prepared.edge_count(), committed_edges, tty);
    let mut edge_stream = prepared.edge_stream()?;
    run_edge_phase(
        &mut edge_stream,
        transport,
        graph,
        key,
        committed_edges,
        &mut chunk_index,
        &id_by_source,
        &mut edge_phase,
    )?;
    edge_phase.finish();

    let finalized = send_command(
        transport,
        BulkLoadCommand::Finalize {
            graph_name: graph.map(str::to_owned),
            client_bulk_key: key.to_owned(),
        },
    )?;
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

/// Rows already committed by a prior run, in artifact order, together with their recorded vertex
/// ids (one per committed row, in commit order).
struct CommittedVertices {
    rows: usize,
    ids: Vec<Vec<u8>>,
}

/// Rows to accumulate into one candidate chunk window before fitting. With a hint the window is
/// the previously fitted count (steady-state chunk size); without one, a generous initial batch
/// lets the first estimate extrapolate beyond the 96-row measurement sample.
fn chunk_accumulation_target(hint: Option<SizeHint>) -> usize {
    hint.map(|hint| hint.entry_count)
        .unwrap_or(INITIAL_CHUNK_ROWS)
}

// ──── progress ────

/// Live progress for one load phase (vertices or edges), rendered through the shared one-line
/// progress renderer: rewritten in place on a terminal, printed only when the percentage
/// advances when captured so logs stay readable.
struct PhaseProgress {
    label: &'static str,
    total: usize,
    committed: usize,
    line: ProgressLine,
}

impl PhaseProgress {
    fn new(label: &'static str, total: usize, committed: usize, tty: bool) -> Self {
        let mut progress = Self {
            label,
            total,
            committed,
            line: ProgressLine::new(tty),
        };
        progress.render();
        progress
    }

    /// Record `committed` more rows and redraw.
    fn advance(&mut self, committed: usize) {
        self.committed += committed;
        self.render();
    }

    /// Terminate the in-place terminal line after the phase reached its final state.
    fn finish(&mut self) {
        self.line.close();
    }

    fn render(&mut self) {
        let percent = self
            .committed
            .saturating_mul(100)
            .checked_div(self.total)
            .unwrap_or(100) as u8;
        self.line.render(percent, &self.line_text(percent));
    }

    fn line_text(&self, percent: u8) -> String {
        format!(
            "loading {:<8} [{}] {} / {} ({:>3}%)",
            self.label,
            crate::progress::bar(percent),
            thousands(self.committed),
            thousands(self.total),
            percent
        )
    }
}

fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Stream vertices into budget-fitted chunks. Rows committed by a prior run are matched to their
/// recorded ids without re-dispatch; the uncommitted tail of a budget-truncated chunk stays
/// buffered for the next Append.
#[allow(
    clippy::too_many_arguments,
    reason = "phase context passed explicitly for testability"
)]
fn run_vertex_phase(
    source: &mut RowStream<'_, VertexRow>,
    transport: &mut impl BulkLoadTransport,
    graph: Option<&str>,
    key: &str,
    committed: CommittedVertices,
    chunk_index: &mut u32,
    id_by_source: &mut HashMap<String, Vec<u8>>,
    progress: &mut PhaseProgress,
) -> Result<(), LoadError> {
    let mut committed_ids = committed.ids.into_iter();
    let mut committed_remaining = committed.rows;
    let mut buffer: Vec<VertexRow> = Vec::new();
    let mut hint: Option<SizeHint> = None;
    loop {
        let target = chunk_accumulation_target(hint);
        let mut accumulated_bytes = 0usize;
        while buffer.len() < target && accumulated_bytes < MAX_ACCUMULATED_RAW_BYTES {
            match source.next_row_with_bytes()? {
                Some((row, bytes)) => {
                    if committed_remaining > 0 {
                        let id = committed_ids.next().ok_or_else(|| {
                            LoadError::Remote(
                                "resume receipts do not cover the committed vertex rows".into(),
                            )
                        })?;
                        id_by_source.insert(row.source_id.clone(), id);
                        committed_remaining -= 1;
                    } else {
                        accumulated_bytes += bytes;
                        buffer.push(row);
                    }
                }
                None => break,
            }
        }
        if buffer.is_empty() {
            break;
        }
        let candidate_count = fit_candidate(buffer.len(), hint, |count| {
            let chunk = vertex_chunk(&buffer[..count])?;
            encode_append_command(graph, key, *chunk_index, chunk)
        })?;
        let chunk = vertex_chunk(&buffer[..candidate_count])?;
        let response = send_command(
            transport,
            BulkLoadCommand::Append {
                graph_name: graph.map(str::to_owned),
                client_bulk_key: key.to_owned(),
                chunk_index: *chunk_index,
                chunk,
            },
        )?;
        let next_offset = match response {
            BulkLoadResponse::Appended {
                next_offset,
                receipt,
                ..
            } => {
                for (row, id) in buffer[..next_offset as usize]
                    .iter()
                    .zip(receipt.allocated_vertex_ids.iter())
                {
                    id_by_source.insert(row.source_id.clone(), id.clone());
                }
                next_offset
            }
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
        buffer.drain(..next_offset as usize);
        *chunk_index += 1;
        progress.advance(next_offset as usize);
        hint = Some(SizeHint::new(candidate_count));
    }
    Ok(())
}

/// Stream edges into budget-fitted chunks, resolving each endpoint against the vertex ids
/// allocated during the vertex phase. Rows committed by a prior run are skipped without parsing.
#[allow(
    clippy::too_many_arguments,
    reason = "phase context passed explicitly for testability"
)]
fn run_edge_phase(
    source: &mut RowStream<'_, EdgeRow>,
    transport: &mut impl BulkLoadTransport,
    graph: Option<&str>,
    key: &str,
    committed_rows: usize,
    chunk_index: &mut u32,
    id_by_source: &HashMap<String, Vec<u8>>,
    progress: &mut PhaseProgress,
) -> Result<(), LoadError> {
    source.skip(committed_rows)?;
    let mut buffer: Vec<EdgeRow> = Vec::new();
    let mut hint: Option<SizeHint> = None;
    loop {
        let target = chunk_accumulation_target(hint);
        let mut accumulated_bytes = 0usize;
        while buffer.len() < target && accumulated_bytes < MAX_ACCUMULATED_RAW_BYTES {
            match source.next_row_with_bytes()? {
                Some((row, bytes)) => {
                    accumulated_bytes += bytes;
                    buffer.push(row);
                }
                None => break,
            }
        }
        if buffer.is_empty() {
            break;
        }
        let candidate_count = fit_candidate(buffer.len(), hint, |count| {
            let chunk = edge_chunk(&buffer[..count], id_by_source)?;
            encode_append_command(graph, key, *chunk_index, chunk)
        })?;
        let chunk = edge_chunk(&buffer[..candidate_count], id_by_source)?;
        let response = send_command(
            transport,
            BulkLoadCommand::Append {
                graph_name: graph.map(str::to_owned),
                client_bulk_key: key.to_owned(),
                chunk_index: *chunk_index,
                chunk,
            },
        )?;
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
        buffer.drain(..next_offset as usize);
        *chunk_index += 1;
        progress.advance(next_offset as usize);
        hint = Some(SizeHint::new(candidate_count));
    }
    Ok(())
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
            let source = resolve_edge_endpoint(&row.source, id_by_source)?;
            let target = resolve_edge_endpoint(&row.target, id_by_source)?;
            Ok(BulkLoadEdgeV1 {
                source,
                target,
                directed: row.directed,
                edge_label_name: Some(row.label.clone()),
                inline_property: row.inline_value.as_ref().map(encode_value).transpose()?,
                initial_edge_properties: encode_properties(&row.properties)?,
            })
        })
        .collect::<Result<Vec<_>, LoadError>>()?;
    Ok(BulkLoadChunkV1::Edges(items))
}

/// Build one wire endpoint: a `source_id` reference becomes an encoded existing vertex ID from
/// the vertices loaded in this artifact; a property endpoint carries the sortable index key.
fn resolve_edge_endpoint(
    endpoint: &EdgeEndpointRow,
    id_by_source: &HashMap<String, Vec<u8>>,
) -> Result<BulkLoadEndpointV1, LoadError> {
    match endpoint {
        EdgeEndpointRow::SourceId(source_id) => {
            let encoded = id_by_source.get(source_id).ok_or_else(|| {
                LoadError::Artifact(format!(
                    "edge endpoint {source_id:?} lacks an encoded vertex id"
                ))
            })?;
            Ok(BulkLoadEndpointV1::Existing(encoded.clone()))
        }
        EdgeEndpointRow::ByProperty(property) => {
            let value = value_to_index_key_bytes(&property.value)
                .map_err(|error| {
                    LoadError::Artifact(format!("edge endpoint by property value encode: {error}"))
                })?
                .ok_or_else(|| {
                    LoadError::Artifact(
                        "edge endpoint by property value is not a sortable (indexable) type".into(),
                    )
                })?;
            Ok(BulkLoadEndpointV1::ByProperty(BulkLoadPropertyEndpointV1 {
                vertex_label: property.label.clone(),
                property_name: property.property.clone(),
                value,
            }))
        }
    }
}

// ──── entry point ────

/// The base job key: the explicit `--key` (or `[load] key`) or the built-in default.
fn base_key(args: &LoadArgs) -> &str {
    args.key.as_deref().unwrap_or(DEFAULT_BULK_KEY)
}

/// Resolve the effective job key: the state-file record (resume/skip identity), a fresh derived
/// key under `--fresh`, or the base `--key`.
fn effective_key(args: &LoadArgs) -> Result<String, LoadError> {
    if args.fresh {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        return Ok(format!("{}.{nonce}", base_key(args)));
    }
    if let Some(path) = &args.state_file
        && let Some(state) = read_state_file(path)?
    {
        return Ok(state.bulk_key);
    }
    Ok(base_key(args).to_owned())
}

/// Validate and run one `gleaph load` invocation.
pub fn execute(args: &LoadArgs) -> Result<LoadOutcome, LoadError> {
    let canister = args
        .canister
        .as_deref()
        .ok_or_else(|| LoadError::Usage("--canister is required".into()))?;
    let input = resolve_input(args)?;
    let prepared = prepare_artifact(&input, args.graph.as_deref(), base_key(args))?;
    let key = effective_key(args)?;
    let mut transport = RemoteBulkLoadTransport::connect(
        canister,
        args.network
            .as_deref()
            .unwrap_or(crate::config::DEFAULT_NETWORK),
        args.identity.as_deref(),
        args.fetch_root_key.unwrap_or(false),
    )?;
    let outcome = run_load(
        &mut transport,
        &prepared,
        args.graph.as_deref(),
        &key,
        args.state_file.as_deref(),
    )?;
    Ok(outcome)
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
            canister: Some("aaaaa-aa".into()),
            graph: None,
            key: Some("test-key".into()),
            network: Some("local".into()),
            identity: None,
            fetch_root_key: Some(false),
            format: None,
            vertices: None,
            edges: None,
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
            source: EdgeEndpointRow::SourceId(source.into()),
            target: EdgeEndpointRow::SourceId(target.into()),
            label: label.into(),
            directed: true,
            inline_value: None,
            properties: Properties::default(),
        }
    }

    fn property_endpoint(label: &str, property: &str, value: Value) -> PropertyEndpointRow {
        PropertyEndpointRow {
            label: label.into(),
            property: property.into(),
            value,
        }
    }

    #[test]
    fn validates_unique_source_ids_and_endpoint_resolution() {
        let artifact = LoadArtifact {
            vertices: vec![vertex("a", &["Person"]), vertex("a", &["Person"])],
            edges: Vec::new(),
        };
        let error = validate_artifact(&artifact).expect_err("duplicate source_id must be rejected");
        assert!(error.to_string().contains("duplicate vertex source_id"));

        let artifact = LoadArtifact {
            vertices: vec![vertex("a", &["Person"])],
            edges: vec![edge("a", "missing", "KNOWS")],
        };
        let error = validate_artifact(&artifact).expect_err("unresolved endpoint must be rejected");
        assert!(error.to_string().contains("does not resolve"));

        let artifact = LoadArtifact {
            vertices: vec![vertex("a", &[])],
            edges: Vec::new(),
        };
        let error = validate_artifact(&artifact).expect_err("empty labels must be rejected");
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
        let (artifact, digest) =
            read_single_file(&path, Format::Json).expect("parse JSON artifact");
        assert_eq!(artifact.vertices.len(), 1);
        assert_eq!(artifact.vertices[0].properties.0.len(), 2);
        assert_eq!(
            artifact.vertices[0].properties.0[1].1,
            Value::DateTime(1_700_000_000, 5)
        );
        assert!(!artifact.edges[0].directed);
        assert_eq!(digest.len(), 64, "single-file digest must be sha256 hex");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn parses_yaml_artifact() {
        let path = temp_path("artifact.yaml");
        write_temp(
            &path,
            "format_version: 1\nvertices:\n  - source_id: v1\n    labels: [Person]\n    properties:\n      age: {Int64: 30}\n",
        );
        let (artifact, _) = read_single_file(&path, Format::Yaml).expect("parse YAML artifact");
        assert_eq!(artifact.vertices[0].properties.0[0].1, Value::Int64(30));
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn scans_jsonl_artifacts_streaming_and_hashes_raw_bytes() {
        let vertices = temp_path("vertices.jsonl");
        let edges = temp_path("edges.jsonl");
        let vertices_content = "{\"source_id\":\"v1\",\"labels\":[\"Person\"]}\n\n{\"source_id\":\"v2\",\"labels\":[\"Person\"]}\n";
        let edges_content = "{\"source\":\"v1\",\"target\":\"v2\",\"label\":\"KNOWS\"}\n";
        write_temp(&vertices, vertices_content);
        write_temp(&edges, edges_content);
        let scan = scan_njson(Some(&vertices), Some(&edges)).expect("scan NDJSON artifacts");
        assert_eq!(scan.vertex_count, 2);
        // The digest hashes the raw file bytes in vertex-then-edge order (state-file identity).
        let mut hasher = Sha256::new();
        hasher.update(vertices_content.as_bytes());
        hasher.update(edges_content.as_bytes());
        assert_eq!(scan.digest, hex_digest(&hasher.finalize()));
        fs::remove_file(vertices).expect("cleanup");
        fs::remove_file(edges).expect("cleanup");
    }

    #[test]
    fn scan_reports_ndjson_parse_error_line_numbers() {
        let vertices = temp_path("bad-vertices.jsonl");
        write_temp(
            &vertices,
            "{\"source_id\":\"v1\",\"labels\":[\"Person\"]}\nnot-json\n",
        );
        let error =
            scan_njson(Some(&vertices), None).expect_err("a malformed NDJSON row must be rejected");
        assert!(error.to_string().contains("line 2"), "{error}");
        fs::remove_file(vertices).expect("cleanup");
    }

    #[test]
    fn rejects_unknown_format_version_and_duplicate_properties() {
        let path = temp_path("bad-version.json");
        write_temp(&path, r#"{"format_version": 2, "vertices": []}"#);
        let error = read_single_file(&path, Format::Json)
            .expect_err("unknown format_version must be rejected");
        assert!(error.to_string().contains("format_version"));
        fs::remove_file(path).expect("cleanup");

        let path = temp_path("dup-props.json");
        write_temp(
            &path,
            r#"{"format_version": 1, "vertices": [{"source_id": "v1", "labels": ["P"], "properties": {"a": {"Int64": 1}, "a": {"Int64": 2}}}]}"#,
        );
        let error = read_single_file(&path, Format::Json)
            .expect_err("duplicate property names must be rejected");
        assert!(error.to_string().contains("duplicate property"));
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn fresh_key_is_derived_and_state_file_reuses_the_recorded_key() {
        let mut args = load_args(&[]);
        args.fresh = true;
        args.key = Some("base".into());
        let fresh = effective_key(&args).expect("fresh key");
        assert!(fresh.starts_with("base."), "fresh key must derive: {fresh}");

        let state = temp_path("state.json");
        write_temp(
            &state,
            r#"{"format_version": 1, "artifact_sha256": "abc", "bulk_key": "recorded-key", "graph": null}"#,
        );
        let mut args = load_args(&[]);
        args.state_file = Some(state.clone());
        args.key = Some("base".into());
        let reused = effective_key(&args).expect("recorded key");
        assert_eq!(reused, "recorded-key");
        fs::remove_file(state).expect("cleanup");
    }

    #[test]
    fn vertices_only_flag_scans_a_single_jsonl_file() {
        let path = temp_path("vertices-only.jsonl");
        write_temp(&path, "{\"source_id\":\"v1\",\"labels\":[\"Person\"]}\n");
        let mut args = load_args(&[]);
        args.vertices = Some(path.clone());
        let input = resolve_input(&args).expect("resolve");
        let prepared = prepare_artifact(&input, None, "k").expect("prepare");
        assert_eq!(prepared.vertex_count(), 1);
        let PreparedLoad::Njson { edges: None, .. } = &prepared else {
            panic!("vertices-only must prepare an NDJSON load without edges");
        };
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn edge_chunk_builds_by_property_endpoints_with_index_keys() {
        let row = EdgeRow {
            source: EdgeEndpointRow::ByProperty(property_endpoint(
                "Person",
                "email",
                Value::Text("a@b.c".into()),
            )),
            target: EdgeEndpointRow::SourceId("v2".into()),
            label: "KNOWS".into(),
            directed: true,
            inline_value: None,
            properties: Properties::default(),
        };
        let mut id_by_source = HashMap::new();
        id_by_source.insert("v2".to_owned(), vec![7; 32]);
        let BulkLoadChunkV1::Edges(items) =
            edge_chunk(&[row], &id_by_source).expect("build edge chunk")
        else {
            panic!("edge chunk");
        };
        let item = &items[0];
        let BulkLoadEndpointV1::ByProperty(property) = &item.source else {
            panic!("property endpoint");
        };
        assert_eq!(property.vertex_label, "Person");
        assert_eq!(property.property_name, "email");
        assert_eq!(
            property.value,
            value_to_index_key_bytes(&Value::Text("a@b.c".into()))
                .expect("indexable")
                .expect("text is indexable")
        );
        assert_eq!(item.target, BulkLoadEndpointV1::Existing(vec![7; 32]));
    }

    #[test]
    fn send_command_classifies_router_invalid_argument_as_operator() {
        struct RejectingTransport;
        impl BulkLoadTransport for RejectingTransport {
            fn status(
                &mut self,
                _graph: Option<&str>,
                _key: &str,
                _cursor: Option<u32>,
                _max_receipts: u32,
            ) -> Result<Result<BulkLoadStatusPage, RouterError>, String> {
                unreachable!("status is not called")
            }

            fn command(
                &mut self,
                _command: BulkLoadCommand,
            ) -> Result<Result<BulkLoadResponse, RouterError>, String> {
                Ok(Err(RouterError::InvalidArgument(
                    "no active vertex property index namespace for property 3".into(),
                )))
            }

            fn ensure_properties(
                &mut self,
                _graph: &str,
                _names: &[String],
            ) -> Result<Result<(), RouterError>, String> {
                unreachable!("ensure_properties is not called")
            }
        }
        let error = send_command(
            &mut RejectingTransport,
            BulkLoadCommand::Append {
                graph_name: None,
                client_bulk_key: "k".into(),
                chunk_index: 0,
                chunk: BulkLoadChunkV1::Vertices(Vec::new()),
            },
        )
        .expect_err("InvalidArgument must surface as operator action");
        assert!(matches!(error, LoadError::Operator(_)));
        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("property index"));
    }

    #[test]
    fn edges_only_artifact_rejects_source_id_endpoints_and_accepts_property_endpoints() {
        let path = temp_path("edges-only.jsonl");
        write_temp(
            &path,
            "{\"source\":\"v1\",\"target\":\"v2\",\"label\":\"KNOWS\"}\n",
        );
        let mut args = load_args(&[]);
        args.edges = Some(path.clone());
        let input = resolve_input(&args).expect("resolve");
        let error = prepare_artifact(&input, None, "k")
            .expect_err("edges-only source_id endpoint must be rejected");
        assert!(error.to_string().contains("has no vertices"), "{error}");
        fs::remove_file(path).expect("cleanup");

        let path = temp_path("edges-only-property.jsonl");
        write_temp(
            &path,
            "{\"source\":{\"label\":\"Person\",\"property\":\"email\",\"value\":{\"Text\":\"a@b.c\"}},\"target\":{\"label\":\"Person\",\"property\":\"email\",\"value\":{\"Text\":\"c@d.e\"}},\"label\":\"KNOWS\"}\n",
        );
        let mut args = load_args(&[]);
        args.edges = Some(path.clone());
        let input = resolve_input(&args).expect("resolve");
        prepare_artifact(&input, None, "k").expect("edges-only property endpoints must validate");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn flags_and_positionals_are_mutually_exclusive() {
        let mut args = load_args(&["v.jsonl"]);
        args.vertices = Some("v.jsonl".into());
        let error = resolve_input(&args).expect_err("mixing flags and positionals must fail");
        assert!(error.to_string().contains("cannot combine"));
    }

    #[test]
    fn single_positional_jsonl_is_ambiguous() {
        let args = load_args(&["v.jsonl"]);
        let error = resolve_input(&args).expect_err("a bare .jsonl positional is ambiguous");
        assert!(error.to_string().contains("--vertices or --edges"));
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
        /// `(graph, names)` recorded by every `ensure_properties` call, in call order.
        interned: Vec<(String, Vec<String>)>,
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

        fn command(
            &mut self,
            command: BulkLoadCommand,
        ) -> Result<Result<BulkLoadResponse, RouterError>, String> {
            match command {
                BulkLoadCommand::Start { .. } => match &self.job {
                    Some(job) => Ok(Ok(BulkLoadResponse::Started {
                        next_chunk_index: job.next_chunk_index,
                    })),
                    None => {
                        self.job = Some(FakeJob {
                            next_chunk_index: 0,
                            receipts: Vec::new(),
                            state: BulkLoadPublicStateV1::Open,
                            next_vertex_ordinal: 0,
                        });
                        Ok(Ok(BulkLoadResponse::Started {
                            next_chunk_index: 0,
                        }))
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
                    Ok(Ok(BulkLoadResponse::Appended {
                        chunk_index,
                        next_offset: commit as u32,
                        receipt,
                    }))
                }
                BulkLoadCommand::Finalize { .. } => {
                    let job = self.job.as_mut().ok_or("finalize without start")?;
                    job.state = BulkLoadPublicStateV1::FinalizePending;
                    Ok(Ok(BulkLoadResponse::FinalizeAccepted {
                        state: job.state.clone(),
                    }))
                }
                BulkLoadCommand::Abort { .. } => {
                    let job = self.job.as_mut().ok_or("abort without start")?;
                    job.state = BulkLoadPublicStateV1::AbortPending;
                    Ok(Ok(BulkLoadResponse::AbortAccepted {
                        state: job.state.clone(),
                    }))
                }
            }
        }

        fn ensure_properties(
            &mut self,
            graph: &str,
            names: &[String],
        ) -> Result<Result<(), RouterError>, String> {
            self.interned.push((graph.to_owned(), names.to_vec()));
            Ok(Ok(()))
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

    fn prepared_single(artifact: LoadArtifact) -> PreparedLoad {
        PreparedLoad::SingleFile {
            property_names: collect_property_names(&artifact.vertices, &artifact.edges),
            artifact,
            digest: "digest".into(),
        }
    }

    #[test]
    fn loader_loads_vertices_then_edges_and_loops_on_next_offset() {
        let prepared = prepared_single(sample_artifact(5, 2));
        let mut transport = FakeBulkLoadTransport {
            job: None,
            budget: 3, // forces multiple Append calls per phase
            interned: Vec::new(),
        };
        let outcome =
            run_load(&mut transport, &prepared, None, "k", None).expect("load should complete");
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
    fn loader_interns_artifact_properties_before_start() {
        // Data-driven properties must be declared (interned) before the first chunk (ADR 0059);
        // run_load issues one batch ensure_properties call before Start.
        let mut artifact = sample_artifact(2, 1);
        artifact.vertices[0].properties = Properties(vec![
            ("demo_graph".into(), Value::Text("social".into())),
            ("name".into(), Value::Text("alice".into())),
        ]);
        artifact.edges[0].properties =
            Properties(vec![("demo_kind".into(), Value::Text("follows".into()))]);
        let prepared = prepared_single(artifact);
        let mut transport = FakeBulkLoadTransport {
            job: None,
            budget: usize::MAX,
            interned: Vec::new(),
        };
        run_load(&mut transport, &prepared, Some("social"), "k", None).expect("load completes");
        assert_eq!(
            transport.interned,
            vec![(
                "social".to_owned(),
                vec![
                    "demo_graph".to_owned(),
                    "demo_kind".to_owned(),
                    "name".to_owned()
                ]
            )]
        );
        assert!(
            transport.job.is_some(),
            "Start must follow the interning call"
        );
    }

    #[test]
    fn loader_requires_graph_name_to_intern_artifact_properties() {
        let mut artifact = sample_artifact(1, 0);
        artifact.vertices[0].properties =
            Properties(vec![("demo_graph".into(), Value::Text("social".into()))]);
        let prepared = prepared_single(artifact);
        let mut transport = FakeBulkLoadTransport {
            job: None,
            budget: usize::MAX,
            interned: Vec::new(),
        };
        let error = run_load(&mut transport, &prepared, None, "k", None)
            .expect_err("interning requires a graph name");
        assert!(error.to_string().contains("no graph name"));
        assert!(transport.interned.is_empty());
        assert!(
            transport.job.is_none(),
            "no Start before the interning check"
        );
    }

    #[test]
    fn loader_resumes_from_receipts_without_reloading_committed_prefix() {
        let prepared = prepared_single(sample_artifact(5, 2));
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
            interned: Vec::new(),
        };
        let outcome =
            run_load(&mut transport, &prepared, None, "k", None).expect("resume should complete");
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
        let prepared = prepared_single(sample_artifact(1, 0));
        let mut transport = FakeBulkLoadTransport {
            job: Some(FakeJob {
                next_chunk_index: 1,
                receipts: Vec::new(),
                state: BulkLoadPublicStateV1::Completed,
                next_vertex_ordinal: 1,
            }),
            budget: usize::MAX,
            interned: Vec::new(),
        };
        let state = temp_path("state.json");
        write_temp(
            &state,
            r#"{"format_version": 1, "artifact_sha256": "digest", "bulk_key": "k", "graph": null}"#,
        );
        let outcome = run_load(&mut transport, &prepared, None, "k", Some(&state))
            .expect("matching completed job must skip");
        assert_eq!(outcome, LoadOutcome::Skipped { key: "k".into() });
        fs::remove_file(state).expect("cleanup");
    }

    #[test]
    fn loader_rejects_completed_job_when_the_artifact_changed() {
        let prepared = PreparedLoad::SingleFile {
            property_names: BTreeSet::new(),
            artifact: sample_artifact(1, 0),
            digest: "new-digest".into(),
        };
        let mut transport = FakeBulkLoadTransport {
            job: Some(FakeJob {
                next_chunk_index: 1,
                receipts: Vec::new(),
                state: BulkLoadPublicStateV1::Completed,
                next_vertex_ordinal: 1,
            }),
            budget: usize::MAX,
            interned: Vec::new(),
        };
        let state = temp_path("state.json");
        write_temp(
            &state,
            r#"{"format_version": 1, "artifact_sha256": "old-digest", "bulk_key": "k", "graph": null}"#,
        );
        let error = run_load(&mut transport, &prepared, None, "k", Some(&state))
            .expect_err("changed artifact must not be silently skipped");
        assert_eq!(error.exit_code(), 1);
        fs::remove_file(state).expect("cleanup");
    }

    #[test]
    fn loader_rejects_a_terminal_failed_job() {
        let prepared = prepared_single(sample_artifact(1, 0));
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
            interned: Vec::new(),
        };
        let error = run_load(&mut transport, &prepared, None, "k", None)
            .expect_err("terminal failed job must be reported");
        assert!(error.to_string().contains("use a new --key"));
        assert_eq!(error.exit_code(), 1);
    }

    fn write_njson_artifact(vertices: usize, edges: usize) -> (PathBuf, PathBuf) {
        let vertices_path = temp_path("stream-vertices.jsonl");
        let edges_path = temp_path("stream-edges.jsonl");
        let mut vertices_content = String::new();
        for index in 0..vertices {
            vertices_content.push_str(&format!(
                "{{\"source_id\":\"v{index}\",\"labels\":[\"Person\"]}}\n"
            ));
        }
        let mut edges_content = String::new();
        for index in 0..edges {
            edges_content.push_str(&format!(
                "{{\"source\":\"v0\",\"target\":\"v{}\",\"label\":\"KNOWS\"}}\n",
                index + 1
            ));
        }
        write_temp(&vertices_path, &vertices_content);
        write_temp(&edges_path, &edges_content);
        (vertices_path, edges_path)
    }

    #[test]
    fn streaming_loader_loads_ndjson_files_with_budget_truncation() {
        let (vertices, edges) = write_njson_artifact(5, 2);
        let prepared = prepare_artifact(
            &ArtifactInput::Both {
                vertices: vertices.clone(),
                edges: edges.clone(),
            },
            None,
            "k",
        )
        .expect("prepare streaming artifact");
        let mut transport = FakeBulkLoadTransport {
            job: None,
            budget: 3, // forces multiple Append calls per phase
            interned: Vec::new(),
        };
        let outcome = run_load(&mut transport, &prepared, None, "k", None)
            .expect("streaming load should complete");
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
        fs::remove_file(vertices).expect("cleanup");
        fs::remove_file(edges).expect("cleanup");
    }

    #[test]
    fn streaming_loader_resumes_from_ndjson_receipts() {
        let (vertices, edges) = write_njson_artifact(5, 1);
        let prepared = prepare_artifact(
            &ArtifactInput::Both {
                vertices: vertices.clone(),
                edges: edges.clone(),
            },
            None,
            "k",
        )
        .expect("prepare streaming artifact");
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
            interned: Vec::new(),
        };
        let outcome = run_load(&mut transport, &prepared, None, "k", None)
            .expect("streaming resume should complete");
        assert_eq!(outcome, LoadOutcome::Loaded { key: "k".into() });
        let job = transport.job.expect("job exists");
        let vertex_chunks = job
            .receipts
            .iter()
            .filter(|row| row.receipt.logical_vertex_count > 0)
            .count();
        assert_eq!(vertex_chunks, 3, "resume plus one vertex chunk");
        assert!(matches!(job.state, BulkLoadPublicStateV1::Completed));
        fs::remove_file(vertices).expect("cleanup");
        fs::remove_file(edges).expect("cleanup");
    }
}
