//! Shared dictionary-catalog protocol types (plan 0335).
//!
//! These types are owned by `gleaph-graph-kernel` — the neutral shared crate — so the
//! Provision canister (catalog owner) and the text canister (relay receiver) share one
//! Candid wire shape and one `dict_required` predicate without either depending on the
//! other's implementation. The text canister imports/re-exports these directly; no copy
//! or compatibility wrapper is kept.
//!
//! Provision stores and verifies the ZSTD-compressed container only; the text canister
//! decompresses once at finalize. Both digests are pinned end-to-end: `compressed_digest`
//! (transfer integrity, verified before decompression) and `raw_digest` (content identity
//! in the text canister's `TextMeta`).

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Fixed per-call cap on a compressed dictionary chunk (plan 0335). The cross-subnet
/// inter-canister payload limit is 2 MiB; this leaves Candid/envelope headroom so a
/// full-size chunk plus its metadata stays under the limit. There is deliberately no
/// same-subnet 9.5 MiB branch — the fixed cap is the single operating point.
pub const MAX_DICT_COMPRESSED_CHUNK_BYTES: usize = 1_945_600;

/// True for analyzer ids that carry the stable-resident MPD dictionary container
/// (plan 0332: id 0 multilingual and id 2 mecab both go through the same dictionary
/// machinery; id 1 unicode-bigram is dictionary-free). Unknown ids are NOT treated as
/// dictionary-carrying; admission validation of the analyzer id set stays with the Router.
pub const fn dict_required(analyzer_id: u32) -> bool {
    matches!(analyzer_id, 0 | 2)
}

/// Compressed-mode metadata for one `admin_upload_dict_chunk` call. `None` on the wire
/// selects the existing raw mode (byte-identical behavior); `Some` selects compressed mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CompressedDictUpload {
    /// Declared total compressed length of the whole container (bounds the compressed
    /// staging region on the receiver).
    pub compressed_len: u64,
}

/// Compressed-mode metadata for one `admin_finalize_dict_upload` call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CompressedDictFinalize {
    /// xxh3_128 over the entire compressed byte stream (verified before decompression).
    pub compressed_digest: u128,
    /// xxh3_128 over the RAW container (pinned as content identity in the text canister).
    pub raw_digest: u128,
    /// Expected decompressed (raw) container length.
    pub raw_len: u64,
}

/// Compressed-mode progress detail nested inside [`DictStatus`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CompressedDictStatus {
    /// xxh3_128 over the compressed bytes received so far; `None`-equivalent before finalize
    /// is represented by the parent `DictStatus.compressed` being `None`.
    pub digest: u128,
    /// Compressed bytes received so far.
    pub received_len: u64,
    /// Declared total compressed length.
    pub total_len: u64,
}

/// Lifecycle of the dictionary blob on the text canister.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DictState {
    /// Fresh open: no dictionary chunks uploaded yet.
    Absent,
    /// Appending chunks; interrupted uploads fail the open loudly (no resume this slice).
    Uploading,
    /// Digest verified and pinned; the pinned tokenizer is resident.
    Finalized,
}

/// Read-only dictionary status (`admin_get_dict_status`). The `compressed` field is
/// `None` in raw mode and `Some` in compressed mode; the raw fields (`state`, `digest`,
/// `len`) describe the raw container in both modes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictStatus {
    pub state: DictState,
    /// xxh3_128 over the raw container bytes; `None` before finalize.
    pub digest: Option<u128>,
    /// Raw container byte length.
    pub len: u64,
    /// Compressed-mode progress; `None` in raw mode.
    pub compressed: Option<CompressedDictStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{decode_args, encode_args};

    #[test]
    fn dict_required_boundaries() {
        assert!(dict_required(0));
        assert!(dict_required(2));
        assert!(!dict_required(1));
        assert!(!dict_required(3));
        assert!(!dict_required(u32::MAX));
    }

    #[test]
    fn compressed_types_candid_roundtrip() {
        let upload = CompressedDictUpload {
            compressed_len: 10_900_552,
        };
        let bytes = encode_args((upload.clone(),)).unwrap();
        let decoded: (CompressedDictUpload,) = decode_args(&bytes).unwrap();
        assert_eq!(decoded.0, upload);

        let finalize = CompressedDictFinalize {
            compressed_digest: u128::MAX,
            raw_digest: 0xDEADBEEF,
            raw_len: 52_930_923,
        };
        let bytes = encode_args((finalize.clone(),)).unwrap();
        let decoded: (CompressedDictFinalize,) = decode_args(&bytes).unwrap();
        assert_eq!(decoded.0, finalize);

        let status = CompressedDictStatus {
            digest: 7,
            received_len: 1_945_600,
            total_len: 10_900_552,
        };
        let bytes = encode_args((status.clone(),)).unwrap();
        let decoded: (CompressedDictStatus,) = decode_args(&bytes).unwrap();
        assert_eq!(decoded.0, status);
    }

    #[test]
    fn dict_status_candid_roundtrip_both_modes() {
        for compressed in [
            None,
            Some(CompressedDictStatus {
                digest: 9,
                received_len: 1_945_600,
                total_len: 10_900_552,
            }),
        ] {
            for state in [
                DictState::Absent,
                DictState::Uploading,
                DictState::Finalized,
            ] {
                let status = DictStatus {
                    state,
                    digest: Some(u128::MAX),
                    len: 52_930_923,
                    compressed: compressed.clone(),
                };
                let bytes = encode_args((status.clone(),)).unwrap();
                let decoded: (DictStatus,) = decode_args(&bytes).unwrap();
                assert_eq!(decoded.0, status);
            }
        }
    }

    #[test]
    fn old_raw_single_arg_decodes_as_none() {
        // Old raw upload wire `(blob)` decodes under the new receiver as `(blob, None)`.
        let raw = encode_args((vec![1u8, 2, 3],)).unwrap();
        let decoded: (Vec<u8>, Option<CompressedDictUpload>) = decode_args(&raw).unwrap();
        assert_eq!(decoded, (vec![1, 2, 3], None));

        // Old raw finalize wire `(nat)` decodes under the new receiver as `(digest, None)`.
        let raw = encode_args((u128::MAX,)).unwrap();
        let decoded: (u128, Option<CompressedDictFinalize>) = decode_args(&raw).unwrap();
        assert_eq!(decoded, (u128::MAX, None));
    }

    #[test]
    fn max_compressed_chunk_candid_stays_under_2_mib() {
        let args = (
            vec![0u8; MAX_DICT_COMPRESSED_CHUNK_BYTES],
            Some(CompressedDictUpload {
                compressed_len: 10_900_552,
            }),
        );
        let bytes = encode_args(args).unwrap();
        assert!(
            bytes.len() <= 2 * 1024 * 1024,
            "max compressed chunk + metadata encoded to {} bytes, exceeding 2 MiB",
            bytes.len()
        );
    }

    #[test]
    fn old_client_decodes_new_dict_status_skipping_unknown_field() {
        // A pre-0335 client that only knows `{ state, digest, len }` must still decode a
        // new `DictStatus` carrying the extra `compressed` field (Candid skips unknown ids).
        #[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize)]
        struct OldDictStatus {
            state: DictState,
            digest: Option<u128>,
            len: u64,
        }

        let new = DictStatus {
            state: DictState::Finalized,
            digest: Some(0xABCD),
            len: 52_930_923,
            compressed: Some(CompressedDictStatus {
                digest: 1,
                received_len: 10_900_552,
                total_len: 10_900_552,
            }),
        };
        let bytes = encode_args((new,)).unwrap();
        let decoded: (OldDictStatus,) = decode_args(&bytes).unwrap();
        assert_eq!(
            decoded.0,
            OldDictStatus {
                state: DictState::Finalized,
                digest: Some(0xABCD),
                len: 52_930_923,
            }
        );
    }
}
