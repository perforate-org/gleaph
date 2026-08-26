//! Pure artifact planning pipeline: bounds validation, chunk splitting, SHA-256 computation.
//!
//! No transport, no I/O: everything here is deterministic over the input bytes so callers can
//! hash-pin an artifact before any network step (ADR 0087 §Canonical upload pipeline).

use sha2::{Digest, Sha256};

use crate::types::{
    ArtifactError, ArtifactId, ArtifactPublishMetadataArgs, CanisterKind, MAX_ARTIFACT_BYTES,
    MAX_ARTIFACT_CHUNKS, MAX_ARTIFACT_SEMANTIC_VERSION_LEN, MAX_CHUNK_BYTES,
};

/// A fully planned artifact ingestion: identity, publish arguments, and the chunk sequence.
///
/// Chunk slices borrow from the planned input, so planning never duplicates the artifact bytes;
/// each ≤1 MiB chunk is copied once at upload time by [`crate::driver::ingest_artifact`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactPlan<'a> {
    /// Canonical wire identity (`canister_kind + semantic_version + sha256`).
    pub artifact_id: ArtifactId,
    /// Arguments to send as `artifact_publish_metadata`.
    pub publish_args: ArtifactPublishMetadataArgs,
    /// Chunk byte slices borrowed from the input, index-aligned with
    /// `publish_args.chunk_hashes`. Every entry except possibly the last is exactly
    /// [`MAX_CHUNK_BYTES`] long.
    pub chunks: Vec<&'a [u8]>,
}

impl ArtifactPlan<'_> {
    /// Number of declared chunks (equal to `publish_args.chunk_hashes.len()`).
    pub fn chunk_count(&self) -> u32 {
        self.chunks.len() as u32
    }

    /// Total declared byte length.
    pub fn byte_length(&self) -> u64 {
        self.publish_args.byte_length
    }
}

/// Validate plan parameters against the mirrored catalog bounds, in the same order and with
/// the same error variants the server's `artifact_publish_metadata` handler applies
/// (`crates/provision/src/canister/mod.rs:637-659`). `pub(crate)` because every parameter is
/// derivable from real bytes via [`plan_artifact`]; exposing it would duplicate the server
/// contract outside this module.
#[allow(clippy::result_large_err)] // ArtifactError is a wire-mirror type; boxing would distort matching.
pub(crate) fn validate_bounds(
    byte_length: u64,
    semantic_version_len: usize,
    chunk_count: u32,
) -> Result<(), ArtifactError> {
    if semantic_version_len > MAX_ARTIFACT_SEMANTIC_VERSION_LEN {
        return Err(ArtifactError::SemanticVersionTooLong {
            max: MAX_ARTIFACT_SEMANTIC_VERSION_LEN as u32,
        });
    }
    if byte_length > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::ArtifactTooLarge {
            max: MAX_ARTIFACT_BYTES,
            byte_length,
        });
    }
    if chunk_count == 0 {
        // Mirrors the server's rejection of empty metadata (zero chunks cannot form an
        // artifact): `crates/provision/src/canister/mod.rs:648-653`.
        return Err(ArtifactError::TooManyChunks {
            max: MAX_ARTIFACT_CHUNKS,
            declared: 0,
        });
    }
    if chunk_count > MAX_ARTIFACT_CHUNKS {
        return Err(ArtifactError::TooManyChunks {
            max: MAX_ARTIFACT_CHUNKS,
            declared: chunk_count,
        });
    }
    Ok(())
}

/// Plan one artifact ingestion from raw bytes: validate the mirrored catalog bounds, split into
/// ≤[`MAX_CHUNK_BYTES`] chunks, compute per-chunk hashes and the full SHA-256, and assemble the
/// publish arguments. Boundary violations are rejected here with the exact
/// [`ArtifactError`] variant the server would reject them with, before any transport call.
#[allow(clippy::result_large_err)] // ArtifactError is a wire-mirror type; boxing would distort matching.
pub fn plan_artifact<'a>(
    bytes: &'a [u8],
    canister_kind: CanisterKind,
    semantic_version: &str,
) -> Result<ArtifactPlan<'a>, ArtifactError> {
    let chunk_count = u64::try_from(bytes.len())
        .map(|len| len.div_ceil(MAX_CHUNK_BYTES as u64))
        .unwrap_or(u64::MAX);
    let chunk_count = u32::try_from(chunk_count).unwrap_or(u32::MAX);
    validate_bounds(bytes.len() as u64, semantic_version.len(), chunk_count)?;

    let chunks: Vec<&[u8]> = bytes.chunks(MAX_CHUNK_BYTES).collect();
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|chunk| sha256(chunk)).collect();
    let full_sha256 = {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    };

    let publish_args = ArtifactPublishMetadataArgs {
        sha256: full_sha256,
        chunk_hashes,
        semantic_version: semantic_version.to_owned(),
        byte_length: bytes.len() as u64,
        canister_kind,
    };
    let artifact_id = ArtifactId::new(canister_kind, semantic_version.to_owned(), full_sha256);

    Ok(ArtifactPlan {
        artifact_id,
        publish_args,
        chunks,
    })
}

/// Compute the SHA-256 digest of `bytes` (same construction as
/// `crates/provision/src/types.rs:876-881`).
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC_SHA256: [u8; 32] = [
        0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22,
        0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00,
        0x15, 0xad,
    ];

    /// NIST vector for one million 'a' bytes.
    const MILLION_A_SHA256: [u8; 32] = [
        0xcd, 0xc7, 0x6e, 0x5c, 0x99, 0x14, 0xfb, 0x92, 0x81, 0xa1, 0xc7, 0xe2, 0x84, 0xd7, 0x3e,
        0x67, 0xf1, 0x80, 0x9a, 0x48, 0xa4, 0x97, 0x20, 0x0e, 0x04, 0x6d, 0x39, 0xcc, 0xc7, 0x11,
        0x2c, 0xd0,
    ];

    #[test]
    fn empty_bytes_rejected_with_zero_declared_chunks() {
        let err = plan_artifact(b"", CanisterKind::Router, "1.0.0").unwrap_err();
        assert_eq!(
            err,
            ArtifactError::TooManyChunks {
                max: MAX_ARTIFACT_CHUNKS,
                declared: 0,
            },
            "empty plans must mirror the server's zero-chunk rejection"
        );
    }

    #[test]
    fn exact_multiple_of_chunk_size_splits_into_equal_chunks() {
        let bytes = vec![7u8; MAX_CHUNK_BYTES * 2];
        let plan = plan_artifact(&bytes, CanisterKind::Graph, "1.0.0").unwrap();
        assert_eq!(plan.chunk_count(), 2);
        assert_eq!(plan.publish_args.chunk_hashes.len(), 2);
        assert!(
            plan.chunks
                .iter()
                .all(|chunk| chunk.len() == MAX_CHUNK_BYTES)
        );
        assert_eq!(plan.byte_length(), (MAX_CHUNK_BYTES * 2) as u64);
    }

    #[test]
    fn remainder_byte_length_produces_shorter_final_chunk() {
        let bytes = vec![9u8; MAX_CHUNK_BYTES * 2 + 7];
        let plan = plan_artifact(&bytes, CanisterKind::TextCanister, "2.0.0").unwrap();
        assert_eq!(plan.chunk_count(), 3);
        assert_eq!(plan.chunks[0].len(), MAX_CHUNK_BYTES);
        assert_eq!(plan.chunks[1].len(), MAX_CHUNK_BYTES);
        assert_eq!(plan.chunks[2].len(), 7);
        // Last-chunk hash must be computed over exactly the remainder bytes.
        assert_eq!(
            plan.publish_args.chunk_hashes[2],
            sha256(b"\x09\x09\x09\x09\x09\x09\x09")
        );
    }

    #[test]
    fn sub_chunk_input_is_a_single_chunk() {
        let plan = plan_artifact(b"abc", CanisterKind::VectorCanister, "0.1.0").unwrap();
        assert_eq!(plan.chunk_count(), 1);
        assert_eq!(plan.chunks[0], b"abc".as_slice());
    }

    #[test]
    fn sha256_matches_known_vector_abc() {
        let plan = plan_artifact(b"abc", CanisterKind::Router, "1.0.0").unwrap();
        assert_eq!(plan.publish_args.sha256, ABC_SHA256);
        // Single-chunk artifacts carry the same digest per chunk and in total.
        assert_eq!(plan.publish_args.chunk_hashes[0], ABC_SHA256);
        assert_eq!(plan.artifact_id.sha256, ABC_SHA256);
    }

    #[test]
    fn sha256_matches_million_a_vector() {
        // NIST vector for one million 'a' bytes; ~0.95 MiB, still a single chunk.
        let bytes = vec![b'a'; 1_000_000];
        let plan = plan_artifact(&bytes, CanisterKind::Graph, "1.0.0").unwrap();
        assert_eq!(plan.chunk_count(), 1);
        assert_eq!(plan.publish_args.sha256, MILLION_A_SHA256);
        assert_eq!(plan.publish_args.chunk_hashes[0], MILLION_A_SHA256);
    }

    #[test]
    fn chunk_hashes_align_with_chunk_slices_across_boundaries() {
        // Multi-chunk inputs must pair every slice with the digest of exactly that slice.
        let bytes = vec![b'a'; MAX_CHUNK_BYTES * 2 + 11];
        let plan = plan_artifact(&bytes, CanisterKind::Graph, "1.0.0").unwrap();
        assert_eq!(plan.chunk_count(), 3);
        for (index, chunk) in plan.chunks.iter().enumerate() {
            let declared = plan.publish_args.chunk_hashes[index];
            assert_eq!(declared, sha256(chunk), "chunk {index} hash misaligned");
            assert_ne!(
                declared, plan.publish_args.sha256,
                "no per-chunk digest may equal the whole-artifact digest here"
            );
        }
    }

    #[test]
    fn oversized_semantic_version_is_rejected() {
        let long_version = "x".repeat(MAX_ARTIFACT_SEMANTIC_VERSION_LEN + 1);
        let err = plan_artifact(b"abc", CanisterKind::Router, &long_version).unwrap_err();
        assert_eq!(
            err,
            ArtifactError::SemanticVersionTooLong {
                max: MAX_ARTIFACT_SEMANTIC_VERSION_LEN as u32,
            }
        );
    }

    #[test]
    fn oversize_byte_length_is_rejected_without_allocating() {
        let err = validate_bounds(MAX_ARTIFACT_BYTES + 1, 1, 1).unwrap_err();
        assert_eq!(
            err,
            ArtifactError::ArtifactTooLarge {
                max: MAX_ARTIFACT_BYTES,
                byte_length: MAX_ARTIFACT_BYTES + 1,
            }
        );
    }

    #[test]
    fn excessive_chunk_count_is_rejected() {
        let err = validate_bounds(1, 1, MAX_ARTIFACT_CHUNKS + 1).unwrap_err();
        assert_eq!(
            err,
            ArtifactError::TooManyChunks {
                max: MAX_ARTIFACT_CHUNKS,
                declared: MAX_ARTIFACT_CHUNKS + 1,
            }
        );
    }

    #[test]
    fn boundary_values_at_the_limits_are_accepted() {
        validate_bounds(
            MAX_ARTIFACT_BYTES,
            MAX_ARTIFACT_SEMANTIC_VERSION_LEN,
            MAX_ARTIFACT_CHUNKS,
        )
        .expect("exact-limit values must pass");
    }

    #[test]
    fn plan_identity_and_publish_args_agree() {
        let bytes = vec![1u8; MAX_CHUNK_BYTES + 3];
        let plan = plan_artifact(&bytes, CanisterKind::PropertyIndex, "4.5.6").unwrap();
        assert_eq!(plan.artifact_id.canister_kind, CanisterKind::PropertyIndex);
        assert_eq!(plan.artifact_id.semantic_version, "4.5.6");
        assert_eq!(plan.publish_args.semantic_version, "4.5.6");
        assert_eq!(plan.publish_args.canister_kind, CanisterKind::PropertyIndex);
        assert_eq!(plan.publish_args.sha256, plan.artifact_id.sha256);
        assert_eq!(plan.publish_args.byte_length, (MAX_CHUNK_BYTES + 3) as u64);
        assert_eq!(
            plan.publish_args.chunk_hashes.len(),
            plan.chunk_count() as usize
        );
    }
}
