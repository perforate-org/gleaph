//! Edge element-id label attribution contract tests (ADR 0090).
//!
//! These are the host-level regression tests for the bug where
//! `ELEMENT_ID(e)` / `GraphPathEdgeId` bytes collided between edges from
//! the same source vertex under different labels with coincident per-bucket
//! slot indices. The contract is: encoding `(shard, owner, label, slot)`
//! yields a unique wire id for every distinct edge in the `(owner, label)`
//! bucket space.

use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeSlotIndex};
use gleaph_graph_kernel::federation::{
    ENCODED_EDGE_ID_BYTES, ElementIdEncodingKey, EncodedEdgeId, ShardId,
};
use gleaph_graph_kernel::path::GraphPathEdgeId;
use ic_stable_lara::VertexId;

const KEY: ElementIdEncodingKey = ElementIdEncodingKey::host_test_fixture();

fn encode(owner: u32, label: u16, slot: u32) -> [u8; ENCODED_EDGE_ID_BYTES] {
    GraphPathEdgeId::new(
        &KEY,
        ShardId::new(0),
        VertexId::from(owner),
        EdgeLabelId::from_raw(label),
        EdgeSlotIndex::from_raw(slot),
    )
    .to_bytes()
}

#[test]
fn edge_element_id_distinguishes_per_label_same_owner_slot() {
    // Four edges from the same owner vertex with different labels and coincident
    // per-bucket slot indices — the alice scenario from the knowledge demo.
    let a = encode(7, 1, 0);
    let b = encode(7, 2, 0);
    let c = encode(7, 3, 0);
    let d = encode(7, 4, 0);
    // Pairwise: every distinct pair must encode to distinct bytes.
    let all = [a, b, c, d];
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            assert_ne!(all[i], all[j], "labels {i} vs {j} collided");
        }
    }
    // The 4 distinct 16-byte words must be 4 distinct values; asserting set size
    // catches a regression where any pair collides.
    let mut sorted = all.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 4, "expected 4 distinct encoded edges");
}

#[test]
fn edge_element_id_distinguishes_within_same_label() {
    // Per-bucket slot indices must still be distinct. This is the regression
    // for the per-label bucket slot model: two edges under the same label with
    // different slots are different edges.
    let a = encode(7, 1, 0);
    let b = encode(7, 1, 1);
    assert_ne!(a, b);
}

#[test]
fn edge_element_id_distinguishes_per_owner_same_label_slot() {
    // Two owners, same label, same slot — must not collide.
    let alice = encode(7, 1, 0);
    let bob = encode(8, 1, 0);
    assert_ne!(alice, bob);
}

#[test]
fn edge_element_id_encoded_length_is_16_bytes() {
    // Pins the wire layout bump (12 → 16) recorded in ADR 0090.
    let bytes = encode(7, 1, 0);
    assert_eq!(bytes.len(), ENCODED_EDGE_ID_BYTES);
    assert_eq!(bytes.len(), 16);
    // Round-trip through `EncodedEdgeId` to confirm the wrapper accepts the new
    // 16-byte width.
    let wrapped = EncodedEdgeId(bytes);
    let back: [u8; ENCODED_EDGE_ID_BYTES] = wrapped.0;
    assert_eq!(back, bytes);
}

#[test]
fn edge_element_id_encoded_differs_from_canonical() {
    // The encoding is not a fixed point for non-zero inputs. The Feistel head
    // mixes the head half under the per-graph key, so a fresh encoding must
    // not equal the raw little-endian canonical bytes.
    let label = EdgeLabelId::from_raw(0x1234);
    let id = gleaph_graph_kernel::federation::GlobalEdgeId::new(
        ShardId::new(0),
        7,
        label,
        EdgeSlotIndex::from_raw(9),
    );
    let encoded = GraphPathEdgeId::from_global(&KEY, id);
    let bytes: [u8; 16] = encoded.to_bytes();
    let canonical = id.to_le_bytes();
    assert_ne!(bytes, canonical);
    // Round trip recovers the canonical form exactly.
    assert_eq!(encoded.decode_global(&KEY), id);
}

#[test]
fn edge_element_id_label_zero_is_distinct_from_label_nonzero() {
    // The bug fix must not silently merge label=0 (label-free reverse edge) with
    // a real label=1 entry under the same (owner, slot).
    let unlabeled = encode(7, 0, 0);
    let labeled = encode(7, 1, 0);
    assert_ne!(unlabeled, labeled);
}
