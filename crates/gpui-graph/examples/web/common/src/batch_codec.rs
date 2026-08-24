//! The example's application-owned payload codec for `SceneMutation::Merge`.
//!
//! The gpui-graph worker protocol leaves batch/patch byte forms to
//! applications by design: `ToWorker::encode_wire_bytes` answers
//! `PayloadCodecRequired` for them rather than guessing an application type's
//! encoding. [`DemoBatchCodec`] is this example's answer — a minimal,
//! fail-closed length-prefixed form for `GraphBatch<String, String, String,
//! String>`.
//!
//! Batch wire form (little-endian; truncation, unknown direction bytes, and
//! trailing bytes are errors, never partial batches): `u64` node count, then
//! per node a length-prefixed UTF-8 key plus its payload string (node data
//! doubles as the display label); then `u64` edge count, then per edge three
//! length-prefixed strings (key, source key, target key), one direction byte,
//! and a length-prefixed label string.

use gpui_graph::frame_source::WireFormatError;
use gpui_graph::worker::{PayloadCodec, SceneMutation};
use gpui_graph::{EdgeDirection, GraphBatch};

const DIRECTION_DIRECTED: u8 = 1;
const DIRECTION_UNDIRECTED: u8 = 2;

/// The example's codec: encodes and decodes `Merge` batches carrying
/// `String` keys/labels. Any other mutation kind has no byte form here and
/// fails closed with `PayloadCodecRequired`.
pub struct DemoBatchCodec;

impl PayloadCodec<String, String, String, String> for DemoBatchCodec {
    fn encode(
        &self,
        mutation: &SceneMutation<String, String, String, String>,
        out: &mut Vec<u8>,
    ) -> Result<(), WireFormatError> {
        match mutation {
            SceneMutation::Merge(batch) => {
                encode_merge_batch(batch, out);
                Ok(())
            }
            _ => Err(WireFormatError::PayloadCodecRequired),
        }
    }

    fn decode(
        &self,
        bytes: &[u8],
    ) -> Result<SceneMutation<String, String, String, String>, WireFormatError> {
        decode_merge_batch(bytes).map(SceneMutation::Merge)
    }
}

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
) -> Result<GraphBatch<String, String, String, String>, WireFormatError> {
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
            other => {
                return Err(WireFormatError::BadDiscriminant {
                    field: "edge direction",
                    value: other,
                });
            }
        };
        let label = take_str(&mut bytes)?;
        edges.push((key, source, target, direction, label));
    }
    if !bytes.is_empty() {
        return Err(WireFormatError::TrailingBytes { extra: bytes.len() });
    }
    Ok(GraphBatch { nodes, edges })
}

fn push_count(out: &mut Vec<u8>, count: usize) {
    let count = u64::try_from(count).expect("element count exceeds u64 range");
    out.extend_from_slice(&count.to_le_bytes());
}

fn read_count(bytes: &mut &[u8]) -> Result<usize, WireFormatError> {
    let raw = u64::from_le_bytes(take(bytes, 8)?.try_into().expect("eight bytes"));
    usize::try_from(raw).map_err(|_| WireFormatError::ExcessiveLength(raw))
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_count(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn take<'a>(bytes: &mut &'a [u8], needed: usize) -> Result<&'a [u8], WireFormatError> {
    if bytes.len() < needed {
        return Err(WireFormatError::Truncated {
            needed,
            remaining: bytes.len(),
        });
    }
    let (head, tail) = bytes.split_at(needed);
    *bytes = tail;
    Ok(head)
}

fn take_byte(bytes: &mut &[u8]) -> Result<u8, WireFormatError> {
    Ok(take(bytes, 1)?[0])
}

fn take_str(bytes: &mut &[u8]) -> Result<String, WireFormatError> {
    let len = read_count(bytes)?;
    let raw = take(bytes, len)?;
    String::from_utf8(raw.to_vec()).map_err(|_| WireFormatError::InvalidUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_batch() -> GraphBatch<String, String, String, String> {
        GraphBatch::new()
            .node("n0".to_string(), "Alice".to_string())
            .node("n1".to_string(), "ボブ ✓".to_string())
            .edge(
                "e0".to_string(),
                "n0".to_string(),
                "n1".to_string(),
                EdgeDirection::Directed,
                "knows".to_string(),
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
            assert_eq!(expected_edge.4, actual_edge.4, "label");
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
    fn corrupted_batch_forms_fail_closed_without_partial_output() {
        // Truncation at every prefix fails instead of yielding a partial batch.
        let mut bytes = Vec::new();
        encode_merge_batch(&sample_batch(), &mut bytes);
        for end in 0..bytes.len() {
            assert!(
                decode_merge_batch(&bytes[..end]).is_err(),
                "prefix of length {end} must not decode"
            );
        }

        // Unknown direction bytes are rejected.
        let mut bytes = Vec::new();
        push_count(&mut bytes, 0); // no nodes
        push_count(&mut bytes, 1); // one edge
        push_str(&mut bytes, "e");
        push_str(&mut bytes, "a");
        push_str(&mut bytes, "b");
        let direction_index = bytes.len();
        bytes.push(DIRECTION_DIRECTED);
        push_str(&mut bytes, "");
        assert!(decode_merge_batch(&bytes).is_ok());

        bytes[direction_index] = 9;
        assert!(
            matches!(
                decode_merge_batch(&bytes),
                Err(WireFormatError::BadDiscriminant {
                    field: "edge direction",
                    value: 9,
                })
            ),
            "a corrupt direction byte must be rejected"
        );

        // Trailing bytes are rejected.
        let mut valid = Vec::new();
        encode_merge_batch(&sample_batch(), &mut valid);
        valid.push(0);
        assert!(matches!(
            decode_merge_batch(&valid),
            Err(WireFormatError::TrailingBytes { extra: 1 })
        ));

        // Invalid UTF-8 in a payload string is rejected by name.
        let mut bad_utf8 = Vec::new();
        push_count(&mut bad_utf8, 1); // one node
        push_count(&mut bad_utf8, 1); // key of length 1
        bad_utf8.push(0xFF);
        assert!(matches!(
            decode_merge_batch(&bad_utf8),
            Err(WireFormatError::InvalidUtf8)
        ));
    }
}
