//! Application-side payload codec for `SceneMutation::Merge` batches.
//!
//! The gpui-graph worker protocol leaves batch/patch byte forms to
//! applications by design: [`gpui_graph::ToWorker::encode_wire_bytes`] returns
//! [`gpui_graph::frame_source::WireFormatError::PayloadCodecRequired`] for
//! them rather than guessing an application type's encoding. This module is
//! the explorer web entry's own choice for `GraphBatch<String, String, String,
//! String>`, and the envelope tags that distinguish application messages from
//! verbatim library requests on the same Worker port.
//!
//! Wire forms (all little-endian, fail-closed on truncation, unknown tags, or
//! trailing bytes):
//!
//! - Envelope: one leading tag byte — [`ENVELOPE_LIB_REQUEST`] means the rest
//!   of the message is exactly a library-encoded request; [`ENVELOPE_MERGE_BATCH`]
//!   means the rest is this module's batch form.
//! - Batch: `u64` node count, then per node a length-prefixed UTF-8 key plus
//!   its payload string (node data doubles as the display label); then `u64`
//!   edge count, then per edge three length-prefixed strings (key, source key,
//!   target key) and one direction byte.

use gpui_graph::{EdgeDirection, GraphBatch};

/// The rest of the message is a library-encoded
/// [`gpui_graph::ToWorker`](gpui_graph::ToWorker) request.
pub const ENVELOPE_LIB_REQUEST: u8 = 1;
/// The rest of the message is a `SceneMutation::Merge` batch in this module's
/// application-owned form.
pub const ENVELOPE_MERGE_BATCH: u8 = 2;

const DIRECTION_DIRECTED: u8 = 1;
const DIRECTION_UNDIRECTED: u8 = 2;

/// Encode one merge batch (without the envelope tag).
pub fn encode_merge_batch(batch: &GraphBatch<String, String, String, String>, out: &mut Vec<u8>) {
    push_count(out, batch.nodes.len());
    for (key, label) in &batch.nodes {
        push_str(out, key);
        push_str(out, label);
    }
    push_count(out, batch.edges.len());
    for (key, source, target, direction, label) in &batch.edges {
        push_str(out, key);
        push_str(out, source);
        push_str(out, target);
        out.push(match direction {
            EdgeDirection::Directed => DIRECTION_DIRECTED,
            EdgeDirection::Undirected => DIRECTION_UNDIRECTED,
        });
        push_str(out, label);
    }
}

/// Decode one merge batch (without the envelope tag). Fail-closed.
pub fn decode_merge_batch(
    bytes: &[u8],
) -> Result<GraphBatch<String, String, String, String>, CodecError> {
    let mut bytes = bytes;
    let node_count = read_count(&mut bytes)?;
    let mut nodes = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let key = take_str(&mut bytes)?;
        let label = take_str(&mut bytes)?;
        nodes.push((key, label));
    }
    let edge_count = read_count(&mut bytes)?;
    let mut edges = Vec::with_capacity(edge_count);
    for _ in 0..edge_count {
        let key = take_str(&mut bytes)?;
        let source = take_str(&mut bytes)?;
        let target = take_str(&mut bytes)?;
        let direction = match take_byte(&mut bytes)? {
            DIRECTION_DIRECTED => EdgeDirection::Directed,
            DIRECTION_UNDIRECTED => EdgeDirection::Undirected,
            other => return Err(CodecError::BadDirection(other)),
        };
        let label = take_str(&mut bytes)?;
        edges.push((key, source, target, direction, label));
    }
    if !bytes.is_empty() {
        return Err(CodecError::TrailingBytes(bytes.len()));
    }
    Ok(GraphBatch { nodes, edges })
}

/// A decode failure. Every case leaves the caller with no partial batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// The declared element count does not fit `usize`.
    ExcessiveLength(u64),
    /// More bytes are required than remain.
    Truncated { needed: usize, remaining: usize },
    /// A length-prefixed string was not valid UTF-8.
    InvalidUtf8,
    /// A direction byte outside the known set.
    BadDirection(u8),
    /// Bytes remained after the declared content.
    TrailingBytes(usize),
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExcessiveLength(raw) => write!(f, "declared count {raw} exceeds usize"),
            Self::Truncated { needed, remaining } => {
                write!(f, "truncated: needed {needed} bytes, {remaining} remain")
            }
            Self::InvalidUtf8 => write!(f, "length-prefixed string was not UTF-8"),
            Self::BadDirection(byte) => write!(f, "unknown direction byte {byte}"),
            Self::TrailingBytes(extra) => write!(f, "{extra} trailing bytes"),
        }
    }
}

fn push_count(out: &mut Vec<u8>, count: usize) {
    let count = u64::try_from(count).expect("element count exceeds u64 range");
    out.extend_from_slice(&count.to_le_bytes());
}

fn read_count(bytes: &mut &[u8]) -> Result<usize, CodecError> {
    let raw = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
    usize::try_from(raw).map_err(|_| CodecError::ExcessiveLength(raw))
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_count(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn take<'a>(bytes: &mut &'a [u8], needed: usize) -> Result<&'a [u8], CodecError> {
    if bytes.len() < needed {
        return Err(CodecError::Truncated {
            needed,
            remaining: bytes.len(),
        });
    }
    let (head, tail) = bytes.split_at(needed);
    *bytes = tail;
    Ok(head)
}

fn take_byte(bytes: &mut &[u8]) -> Result<u8, CodecError> {
    Ok(take(bytes, 1)?[0])
}

fn take_str(bytes: &mut &[u8]) -> Result<String, CodecError> {
    let len = read_count(bytes)?;
    let raw = take(bytes, len)?;
    String::from_utf8(raw.to_vec()).map_err(|_| CodecError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_batch() -> GraphBatch<String, String, String, String> {
        GraphBatch::new()
            .node("n0".to_string(), "Alice".to_string())
            .node("n1".to_string(), "ボブ ✓".to_string())
            .node("n2".to_string(), String::new())
            .edge(
                "e0".to_string(),
                "n0".to_string(),
                "n1".to_string(),
                EdgeDirection::Directed,
                "knows".to_string(),
            )
            .edge(
                "e1".to_string(),
                "n1".to_string(),
                "n2".to_string(),
                EdgeDirection::Undirected,
                String::new(),
            )
    }

    /// Field-wise comparison: `GraphBatch` intentionally has no `PartialEq`.
    fn assert_batches_equal(
        expected: &GraphBatch<String, String, String, String>,
        actual: &GraphBatch<String, String, String, String>,
    ) {
        assert_eq!(expected.nodes.len(), actual.nodes.len());
        for (expected_node, actual_node) in expected.nodes.iter().zip(&actual.nodes) {
            assert_eq!(expected_node, actual_node);
        }
        assert_eq!(expected.edges.len(), actual.edges.len());
        for (expected_edge, actual_edge) in expected.edges.iter().zip(&actual.edges) {
            assert_eq!(expected_edge.0, actual_edge.0, "edge key");
            assert_eq!(expected_edge.1, actual_edge.1, "source key");
            assert_eq!(expected_edge.2, actual_edge.2, "target key");
            assert_eq!(expected_edge.3, actual_edge.3, "direction");
            assert_eq!(expected_edge.4, actual_edge.4, "payload");
        }
    }

    #[test]
    fn merge_batches_round_trip_through_the_application_codec() {
        let batch = sample_batch();
        let mut bytes = Vec::new();
        encode_merge_batch(&batch, &mut bytes);

        let decoded = decode_merge_batch(&bytes).expect("round trip must succeed");
        assert_batches_equal(&batch, &decoded);
    }

    #[test]
    fn empty_batch_round_trip_and_envelope_distinctness() {
        let mut bytes = Vec::new();
        encode_merge_batch(&GraphBatch::default(), &mut bytes);
        let decoded = decode_merge_batch(&bytes).expect("empty batch round trip");
        assert_eq!(decoded.nodes, Vec::new());
        assert_eq!(decoded.edges, Vec::new());
        assert_eq!(ENVELOPE_LIB_REQUEST, 1);
        assert_eq!(ENVELOPE_MERGE_BATCH, 2);
    }

    #[test]
    fn corrupted_batch_forms_fail_closed_without_partial_output() {
        let mut bytes = Vec::new();
        encode_merge_batch(&sample_batch(), &mut bytes);

        // Truncation at every prefix fails instead of yielding a partial batch.
        for end in 0..bytes.len() {
            assert!(
                decode_merge_batch(&bytes[..end]).is_err(),
                "prefix of length {end} must not decode"
            );
        }

        // Unknown direction bytes are rejected.
        let mut bytes = Vec::new();
        push_count(&mut bytes, 0);
        push_count(&mut bytes, 1);
        push_str(&mut bytes, "e");
        push_str(&mut bytes, "a");
        push_str(&mut bytes, "b");
        let direction_index = bytes.len();
        bytes.push(DIRECTION_DIRECTED);
        push_str(&mut bytes, "");
        assert!(decode_merge_batch(&bytes).is_ok());

        bytes[direction_index] = 9;
        assert!(matches!(
            decode_merge_batch(&bytes),
            Err(CodecError::BadDirection(9))
        ));

        // Trailing bytes are rejected.
        let mut valid = Vec::new();
        encode_merge_batch(&sample_batch(), &mut valid);
        valid.push(0);
        assert!(matches!(
            decode_merge_batch(&valid),
            Err(CodecError::TrailingBytes(1))
        ));
    }
}
