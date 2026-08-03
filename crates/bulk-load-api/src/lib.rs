//! Versioned Router bulk-load and ordered atomic-insert wire contract (ADR 0057 / 0060).
//!
//! This crate owns the public Candid wire types shared by the Router, the CLI, and (longer term)
//! the SDK CDK, so every consumer encodes and validates one contract. Router-internal execution
//! logic (classification into shard requests, request fingerprints, response projection from
//! durable records, response-bound preflight) stays in the Router; this crate holds the wire
//! shapes and their pure validation.

use std::collections::BTreeSet;

use candid::{CandidType, Encode};
use gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES;
use gleaph_graph_kernel::index::validate_index_value_key_bytes;
use gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Maximum number of logical operations admitted by one public atomic insert.
pub const MAX_ATOMIC_INSERT_OPERATIONS: usize = 1024;

/// Maximum number of receipt rows returned by one status page.
pub const MAX_BULK_LOAD_RECEIPTS_PER_PAGE: u32 = 64;

/// Router-owned projection of the Graph-specific durable receipts.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertReceiptV1 {
    pub logical_operation_count: u64,
    pub logical_vertex_count: u64,
    pub logical_edge_count: u64,
    /// Opaque graph-scoped encoded IDs in vertex-operation ordinal order.
    /// Edge-only inserts always return an empty list.
    pub allocated_vertex_ids: Vec<Vec<u8>>,
}

impl AtomicInsertReceiptV1 {
    pub fn validate(&self) -> Result<(), String> {
        let expected_operations = self
            .logical_vertex_count
            .checked_add(self.logical_edge_count)
            .ok_or_else(|| "atomic insert receipt logical counts overflow".to_string())?;
        if expected_operations != self.logical_operation_count {
            return Err(
                "atomic insert receipt logical counts must sum to logical operation count".into(),
            );
        }
        let expected_vertex_count = usize::try_from(self.logical_vertex_count)
            .map_err(|_| "atomic insert receipt logical vertex count overflows usize")?;
        if self.allocated_vertex_ids.len() != expected_vertex_count {
            return Err(
                "atomic insert receipt allocated vertex ID count must equal logical vertex count"
                    .into(),
            );
        }
        for (ordinal, id) in self.allocated_vertex_ids.iter().enumerate() {
            if id.len() != ENCODED_VERTEX_ID_BYTES {
                return Err(format!(
                    "atomic insert receipt allocated vertex ID {ordinal} must be exactly {} bytes",
                    ENCODED_VERTEX_ID_BYTES
                ));
            }
        }
        let encoded = Encode!(self)
            .map_err(|error| format!("atomic insert receipt encode failed: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("atomic insert receipt exceeds the safe payload bound".into());
        }
        Ok(())
    }
}

/// One logical operation in an ordered atomic insert.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AtomicInsertOperationV1 {
    Vertex(AtomicInsertVertexV1),
    Edge(AtomicInsertEdgeV1),
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertVertexV1 {
    pub vertex_labels: Vec<String>,
    pub initial_properties: Vec<AtomicInsertPropertyV1>,
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertEdgeV1 {
    pub source: AtomicInsertEndpointV1,
    pub target: AtomicInsertEndpointV1,
    pub directed: bool,
    pub edge_label_name: Option<String>,
    pub inline_property: Option<Vec<u8>>,
    pub initial_edge_properties: Vec<AtomicInsertPropertyV1>,
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum AtomicInsertEndpointV1 {
    Existing(Vec<u8>),
    /// Ordinal among vertex operations, not the position in the mixed operation array.
    NewVertexOrdinal(u32),
}

#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertPropertyV1 {
    pub property_name: String,
    pub value: Vec<u8>,
}

/// Public durable bulk-load command family (ADR 0057).
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadCommand {
    Start {
        graph_name: Option<String>,
        client_bulk_key: String,
    },
    Append {
        graph_name: Option<String>,
        client_bulk_key: String,
        chunk_index: u32,
        chunk: BulkLoadChunkV1,
    },
    Finalize {
        graph_name: Option<String>,
        client_bulk_key: String,
    },
    Abort {
        graph_name: Option<String>,
        client_bulk_key: String,
    },
}

/// Self-contained vertex-only or existing-ID edge-only chunk.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadChunkV1 {
    Vertices(Vec<AtomicInsertVertexV1>),
    Edges(Vec<BulkLoadEdgeV1>),
}

/// One edge in a durable bulk-load chunk.
///
/// Endpoints reference existing vertices either by their graph-scoped encoded ID
/// ([`BulkLoadEndpointV1::Existing`]) or by vertex label + property equality
/// ([`BulkLoadEndpointV1::ByProperty`]); the Router resolves `ByProperty` endpoints through the
/// graph property index before the chunk is admitted (ADR 0060 planned extension).
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadEdgeV1 {
    pub source: BulkLoadEndpointV1,
    pub target: BulkLoadEndpointV1,
    pub directed: bool,
    pub edge_label_name: Option<String>,
    pub inline_property: Option<Vec<u8>>,
    pub initial_edge_properties: Vec<AtomicInsertPropertyV1>,
}

/// One edge endpoint in a durable bulk-load chunk.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadEndpointV1 {
    /// Graph-scoped encoded existing vertex ID (`encode_global_vertex_id` output).
    Existing(Vec<u8>),
    /// Vertex referenced by label + property equality, resolved by the Router through the
    /// converged property index on `(vertex_label, property_name)`.
    ByProperty(BulkLoadPropertyEndpointV1),
}

/// Property-based vertex reference for a bulk-load edge endpoint.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadPropertyEndpointV1 {
    pub vertex_label: String,
    pub property_name: String,
    /// Sortable index key bytes for the GQL value (`gleaph_gql::value_to_index_key_bytes`).
    pub value: Vec<u8>,
}

/// Response to one public bulk-load command.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadResponse {
    Started {
        next_chunk_index: u32,
    },
    Appended {
        chunk_index: u32,
        /// Operations of this candidate batch committed as this chunk. The client resumes the
        /// remainder at `chunk_index + 1` (ADR 0060 `Resumable` execution).
        next_offset: u32,
        receipt: AtomicInsertReceiptV1,
    },
    FinalizeAccepted {
        state: BulkLoadPublicStateV1,
    },
    AbortAccepted {
        state: BulkLoadPublicStateV1,
    },
}

/// Public projection of the durable bulk-load lifecycle. Internal placement and cursors are never
/// exposed through this enum.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadPublicStateV1 {
    Open,
    AppendPending,
    FinalizePending,
    AbortPending,
    Completed,
    Aborted,
    Failed { reason: String },
}

/// One committed chunk receipt in a status page.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadChunkReceiptV1 {
    pub chunk_index: u32,
    pub receipt: AtomicInsertReceiptV1,
}

/// Bounded status response for one graph-scoped durable bulk-load job.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadStatusPage {
    pub state: BulkLoadPublicStateV1,
    pub next_chunk_index: u32,
    pub committed_chunk_count: u32,
    pub completed_chunk_count: u32,
    pub terminal_at_ns: Option<u64>,
    pub expires_at_ns: Option<u64>,
    pub receipts: Vec<BulkLoadChunkReceiptV1>,
    pub next_receipt_cursor: Option<u32>,
}

impl BulkLoadCommand {
    /// Validate the public command before graph resolution, stable lookup, or dispatch.
    pub fn validate(&self) -> Result<(), String> {
        let (graph_name, key) = match self {
            Self::Start {
                graph_name,
                client_bulk_key,
            }
            | Self::Finalize {
                graph_name,
                client_bulk_key,
            }
            | Self::Abort {
                graph_name,
                client_bulk_key,
            }
            | Self::Append {
                graph_name,
                client_bulk_key,
                ..
            } => (graph_name, client_bulk_key),
        };
        if let Some(name) = graph_name
            && (name.is_empty() || name.len() > 256)
        {
            return Err("graph_name must be 1..=256 UTF-8 bytes when present".into());
        }
        if key.is_empty() || key.len() > 256 {
            return Err("client_bulk_key must be 1..=256 UTF-8 bytes".into());
        }
        if let Self::Append { chunk, .. } = self {
            chunk.validate()?;
        }
        let encoded =
            Encode!(self).map_err(|error| format!("bulk-load command encode: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("bulk-load command exceeds the safe payload bound".into());
        }
        Ok(())
    }
}

impl BulkLoadChunkV1 {
    pub fn operation_count(&self) -> usize {
        match self {
            Self::Vertices(items) => items.len(),
            Self::Edges(items) => items.len(),
        }
    }

    /// Validate one self-contained chunk and its exact envelope bound.
    ///
    /// `Resumable` bulk-load chunks (ADR 0060) are not capped by
    /// [`MAX_ATOMIC_INSERT_OPERATIONS`]: the runtime instruction budget decides the committed
    /// prefix, and the message payload bound and the durable receipt-row bound bound the
    /// candidate size.
    pub fn validate(&self) -> Result<(), String> {
        if self.operation_count() == 0 {
            return Err("bulk-load chunk must contain at least one operation".into());
        }
        match self {
            Self::Vertices(items) => {
                let vertex_count = items.len() as u32;
                let operations: Vec<AtomicInsertOperationV1> = items
                    .iter()
                    .cloned()
                    .map(AtomicInsertOperationV1::Vertex)
                    .collect();
                validate_atomic_insert_operations(&operations, vertex_count)?;
            }
            Self::Edges(items) => {
                for (ordinal, item) in items.iter().enumerate() {
                    validate_bulk_load_endpoint(ordinal, "source", &item.source)?;
                    validate_bulk_load_endpoint(ordinal, "target", &item.target)?;
                    if item.edge_label_name.as_deref() == Some("") {
                        return Err(format!(
                            "bulk-load edge {ordinal} contains an empty edge label"
                        ));
                    }
                    if let Some(inline) = &item.inline_property
                        && inline.len() > gleaph_graph_kernel::entry::MAX_EDGE_INLINE_PROPERTY_BYTES
                    {
                        return Err(format!(
                            "bulk-load edge {ordinal} inline property exceeds the byte bound"
                        ));
                    }
                    validate_batch_properties(ordinal, &item.initial_edge_properties)?;
                }
            }
        }
        let encoded = Encode!(self).map_err(|error| format!("bulk-load chunk encode: {error}"))?;
        if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("bulk-load chunk exceeds the safe payload bound".into());
        }
        Ok(())
    }

    /// Compute the canonical chunk fingerprint used by Router admission and durable replay.
    pub fn fingerprint(&self) -> Result<[u8; 32], String> {
        self.validate()?;
        let mut normalized = self.clone();
        match &mut normalized {
            Self::Vertices(items) => {
                for item in items {
                    item.vertex_labels
                        .sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                    item.initial_properties.sort_by(|left, right| {
                        left.property_name
                            .as_bytes()
                            .cmp(right.property_name.as_bytes())
                    });
                }
            }
            Self::Edges(items) => {
                for item in items {
                    item.initial_edge_properties.sort_by(|left, right| {
                        left.property_name
                            .as_bytes()
                            .cmp(right.property_name.as_bytes())
                    });
                }
            }
        }
        let encoded = Encode!(&normalized)
            .map_err(|error| format!("bulk-load chunk fingerprint encode: {error}"))?;
        let mut hasher = Sha256::new();
        hasher.update(b"gleaph:bulk-load-chunk:v1\0");
        hasher.update(encoded);
        Ok(hasher.finalize().into())
    }
}

/// Validate one bulk-load edge endpoint: `Existing` must carry a full-width encoded vertex ID,
/// and `ByProperty` must carry a non-empty label/property plus a valid index-value key.
fn validate_bulk_load_endpoint(
    ordinal: usize,
    name: &str,
    endpoint: &BulkLoadEndpointV1,
) -> Result<(), String> {
    match endpoint {
        BulkLoadEndpointV1::Existing(bytes) if bytes.len() != ENCODED_VERTEX_ID_BYTES => {
            Err(format!(
                "bulk-load edge {ordinal} {name} must be exactly {ENCODED_VERTEX_ID_BYTES} bytes"
            ))
        }
        BulkLoadEndpointV1::Existing(_) => Ok(()),
        BulkLoadEndpointV1::ByProperty(property) => {
            if property.vertex_label.is_empty() || property.vertex_label.len() > 256 {
                return Err(format!(
                    "bulk-load edge {ordinal} {name} vertex label must be 1..=256 bytes"
                ));
            }
            if property.property_name.is_empty() || property.property_name.len() > 256 {
                return Err(format!(
                    "bulk-load edge {ordinal} {name} property name must be 1..=256 bytes"
                ));
            }
            validate_index_value_key_bytes(&property.value)
                .map_err(|error| format!("bulk-load edge {ordinal} {name}: {error}"))?;
            Ok(())
        }
    }
}

/// Validate per-operation labels, properties, and endpoints shared by atomic insert and
/// bulk-load chunks. `vertex_count` bounds `NewVertexOrdinal` endpoints; bulk-load edge chunks
/// pass zero because their endpoints are always `Existing`.
///
/// Public because the Router applies the same operation-level validation to its
/// `AtomicInsertRequestV1` envelope before classification.
pub fn validate_atomic_insert_operations(
    operations: &[AtomicInsertOperationV1],
    vertex_count: u32,
) -> Result<(), String> {
    for (ordinal, operation) in operations.iter().enumerate() {
        match operation {
            AtomicInsertOperationV1::Vertex(item) => {
                if item.vertex_labels.iter().any(String::is_empty) {
                    return Err(format!(
                        "atomic insert operation {ordinal} contains an empty vertex label"
                    ));
                }
                validate_batch_properties(ordinal, &item.initial_properties)?;
            }
            AtomicInsertOperationV1::Edge(item) => {
                validate_batch_endpoint(ordinal, "source", &item.source, vertex_count)?;
                validate_batch_endpoint(ordinal, "target", &item.target, vertex_count)?;
                if item.edge_label_name.as_deref() == Some("") {
                    return Err(format!(
                        "atomic insert operation {ordinal} contains an empty edge label"
                    ));
                }
                if let Some(inline) = &item.inline_property
                    && inline.len() > gleaph_graph_kernel::entry::MAX_EDGE_INLINE_PROPERTY_BYTES
                {
                    return Err(format!(
                        "atomic insert operation {ordinal} inline property exceeds the byte bound"
                    ));
                }
                validate_batch_properties(ordinal, &item.initial_edge_properties)?;
            }
        }
    }
    Ok(())
}

fn validate_batch_endpoint(
    ordinal: usize,
    name: &str,
    endpoint: &AtomicInsertEndpointV1,
    vertex_count: u32,
) -> Result<(), String> {
    match endpoint {
        AtomicInsertEndpointV1::Existing(bytes) if bytes.len() != ENCODED_VERTEX_ID_BYTES => {
            Err(format!(
                "atomic insert operation {ordinal} {name} must be exactly {} bytes",
                ENCODED_VERTEX_ID_BYTES
            ))
        }
        AtomicInsertEndpointV1::NewVertexOrdinal(vertex_ordinal)
            if *vertex_ordinal >= vertex_count =>
        {
            Err(format!(
                "atomic insert operation {ordinal} {name} references unknown vertex ordinal {vertex_ordinal}"
            ))
        }
        _ => Ok(()),
    }
}

fn validate_batch_properties(
    ordinal: usize,
    properties: &[AtomicInsertPropertyV1],
) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for property in properties {
        if property.property_name.is_empty() {
            return Err(format!(
                "atomic insert operation {ordinal} contains an empty property name"
            ));
        }
        if !names.insert(&property.property_name) {
            return Err(format!(
                "atomic insert operation {ordinal} repeats property name {}",
                property.property_name
            ));
        }
        if property.value.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err(format!(
                "atomic insert operation {ordinal} property value exceeds the payload bound"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Decode;

    #[test]
    fn bulk_load_command_round_trips_candid() {
        let command = BulkLoadCommand::Append {
            graph_name: Some("tenant.main".into()),
            client_bulk_key: "k".into(),
            chunk_index: 3,
            chunk: BulkLoadChunkV1::Vertices(vec![AtomicInsertVertexV1 {
                vertex_labels: vec!["Person".into()],
                initial_properties: Vec::new(),
            }]),
        };
        command.validate().expect("valid command");
        let bytes = Encode!(&command).expect("encode");
        let decoded: BulkLoadCommand = candid::Decode!(&bytes, BulkLoadCommand).expect("decode");
        assert_eq!(decoded, command);
    }

    #[test]
    fn chunk_fingerprint_normalizes_property_order() {
        let mut chunk = BulkLoadChunkV1::Vertices(vec![AtomicInsertVertexV1 {
            vertex_labels: vec!["Person".into()],
            initial_properties: vec![
                AtomicInsertPropertyV1 {
                    property_name: "zeta".into(),
                    value: vec![1],
                },
                AtomicInsertPropertyV1 {
                    property_name: "alpha".into(),
                    value: vec![2],
                },
            ],
        }]);
        let first = chunk.fingerprint().expect("fingerprint");
        let BulkLoadChunkV1::Vertices(items) = &mut chunk else {
            panic!("vertex chunk");
        };
        items[0].initial_properties.swap(0, 1);
        assert_eq!(chunk.fingerprint().expect("fingerprint"), first);
    }

    #[test]
    fn edge_chunk_validates_existing_width_and_by_property_endpoints() {
        let by_property = BulkLoadEndpointV1::ByProperty(BulkLoadPropertyEndpointV1 {
            vertex_label: "Person".into(),
            property_name: "email".into(),
            value: b"a@b.c".to_vec(),
        });
        let chunk = BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
            source: BulkLoadEndpointV1::Existing(vec![0; ENCODED_VERTEX_ID_BYTES]),
            target: by_property,
            directed: true,
            edge_label_name: Some("Knows".into()),
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }]);
        chunk
            .validate()
            .expect("existing + by-property endpoints must validate");

        let wrong_width = BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
            source: BulkLoadEndpointV1::Existing(vec![0; ENCODED_VERTEX_ID_BYTES - 1]),
            target: BulkLoadEndpointV1::Existing(vec![0; ENCODED_VERTEX_ID_BYTES]),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }]);
        assert!(
            wrong_width.validate().is_err(),
            "short Existing endpoint must reject"
        );

        let empty_label = BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
            source: BulkLoadEndpointV1::ByProperty(BulkLoadPropertyEndpointV1 {
                vertex_label: String::new(),
                property_name: "email".into(),
                value: vec![1],
            }),
            target: BulkLoadEndpointV1::Existing(vec![0; ENCODED_VERTEX_ID_BYTES]),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }]);
        assert!(
            empty_label.validate().is_err(),
            "empty ByProperty vertex label must reject"
        );
    }

    #[test]
    fn edge_chunk_rejects_by_property_value_beyond_index_key_bound() {
        let oversized = vec![0; gleaph_graph_kernel::index::MAX_INDEX_VALUE_KEY_BYTES + 1];
        let chunk = BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
            source: BulkLoadEndpointV1::ByProperty(BulkLoadPropertyEndpointV1 {
                vertex_label: "Person".into(),
                property_name: "email".into(),
                value: oversized,
            }),
            target: BulkLoadEndpointV1::Existing(vec![0; ENCODED_VERTEX_ID_BYTES]),
            directed: true,
            edge_label_name: None,
            inline_property: None,
            initial_edge_properties: Vec::new(),
        }]);
        let error = chunk
            .validate()
            .expect_err("oversized ByProperty value must reject");
        assert!(error.contains("index value key"), "{error}");
    }

    #[test]
    fn receipt_validate_enforces_id_count_and_width() {
        let receipt = AtomicInsertReceiptV1 {
            logical_operation_count: 1,
            logical_vertex_count: 1,
            logical_edge_count: 0,
            allocated_vertex_ids: vec![vec![0; ENCODED_VERTEX_ID_BYTES]],
        };
        receipt.validate().expect("valid receipt");
        let bad = AtomicInsertReceiptV1 {
            allocated_vertex_ids: vec![vec![0; ENCODED_VERTEX_ID_BYTES - 1]],
            ..receipt.clone()
        };
        assert!(bad.validate().is_err());
    }
}
