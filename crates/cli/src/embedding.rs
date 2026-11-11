//! Deterministic query embeddings for the knowledge demo's vector-search prepared query.
//!
//! The demo ingests synthetic 768-dimensional Document embeddings derived from
//! `sha256(SEED_PREFIX + source_id)` with a 32-bit xorshift expansion, L2-normalized
//! (`demo/knowledge/scripts/embedding-core.mjs`, shared with the browser page). A natural-language
//! team-search query needs the same recipe so the `$query` parameter carries the GQL `Value::Bytes`
//! wire form: 768 little-endian f32 values (6144 bytes).
//!
//! This module is the Rust port of that recipe; the golden test pins it to the JavaScript output.

use sha2::{Digest, Sha256};

/// Embedding dimension shared with the demo vector index.
pub const EMBEDDING_DIMS: usize = 768;

/// Domain-separation prefix shared with the demo's document-embedding generator.
pub const SEED_PREFIX: &str = "gleaph-knowledge-demo-embedding-v1:";

/// Derive the 768-dimensional unit vector for `text` as 6144 little-endian f32 bytes.
pub fn query_vector_bytes(text: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(SEED_PREFIX.as_bytes());
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();

    let mut state = u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]]);
    if state == 0 {
        state = 0x9e37_79b9;
    }

    let mut raw = [0f64; EMBEDDING_DIMS];
    let mut sum_squares = 0f64;
    for value in &mut raw {
        // xorshift32, identical to the JavaScript reference (u32 absorbs the `>>> 0` widening).
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let mapped = f64::from(state) / 2_147_483_648.0 - 1.0;
        *value = mapped;
        sum_squares += mapped * mapped;
    }
    let norm = sum_squares.sqrt();

    let mut bytes = Vec::with_capacity(EMBEDDING_DIMS * 4);
    for value in &raw {
        bytes.extend_from_slice(&((value / norm) as f32).to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_vector_matches_the_javascript_recipe() {
        let bytes = query_vector_bytes("GraphRAG retrieval");
        assert_eq!(bytes.len(), EMBEDDING_DIMS * 4);
        let values: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();
        // Golden values from `unitVectorFromDigest(sha256(SEED_PREFIX + "GraphRAG retrieval"))`.
        let golden = [
            -0.055_040_552_f32,
            0.015_409_667_f32,
            -0.047_910_57_f32,
            -0.022_541_817_f32,
        ];
        for (actual, expected) in values.iter().zip(golden) {
            assert!(
                (actual - expected).abs() < 1e-6,
                "expected {expected}, got {actual}"
            );
        }
        let norm: f32 = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm should be 1, got {norm}");
    }

    #[test]
    fn query_vector_is_deterministic_and_byte_stable() {
        let first = query_vector_bytes("Indexing");
        let second = query_vector_bytes("Indexing");
        assert_eq!(first, second);
        assert_ne!(first, query_vector_bytes("Indexing for traversal"));
    }
}
