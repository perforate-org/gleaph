//! Shared dictionary-catalog protocol types (plan 0335).
//!
//! These types are owned by `gleaph-graph-kernel` — the neutral shared crate — so the
//! Provision canister (catalog owner) and the text canister (relay receiver) share one
//! Candid wire shape and one `dict_required` predicate without either depending on the
//! other's implementation. The text canister imports/re-exports these directly; no copy
//! or compatibility wrapper is kept.
//!
//! Provision stores and verifies the ZSTD-compressed container only; frames decode
//! per relay call on arrival at the text canister, and finalize runs a one-pass
//! region-16 raw hash as the authority. Both digests are pinned end-to-end:
//! per-frame `frame_digest` values (transfer integrity, verified before decode) and
//! `raw_digest` (content identity in the text canister's `TextMeta`).

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Fixed per-call cap on a compressed dictionary chunk (plan 0335). The cross-subnet
/// inter-canister payload limit is 2 MiB; this leaves Candid/envelope headroom so a
/// full-size chunk plus its metadata stays under the limit. There is deliberately no
/// same-subnet 9.5 MiB branch — the fixed cap is the single operating point.
pub const MAX_DICT_COMPRESSED_CHUNK_BYTES: usize = 1_945_600;

/// Dictionary kind selected per index via `WITH DICTIONARY` (plan 0343). The DDL
/// identifiers (`japanese`, `korean`) map to these variants at Router admission; the
/// canonical order is the variant order. `Ord` supports the admission-side `BTreeSet`
/// normal form; the relay iterates the normalized kinds in this order.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, CandidType, Serialize, Deserialize,
)]
pub enum DictKind {
    /// Japanese: ipadic 2.7.0 container.
    Japanese,
    /// Korean: mecab-ko-dic 2.1.1-20180720 container.
    Korean,
}

/// True when the dictionary machinery engages for one (analyzer, kinds) pair
/// (plan 0343: required-AND-selected). Admission owns validity (id 2 requires exactly
/// {Japanese}, id 3 exactly {Korean}, id 1 requires empty, id 0 any subset); this
/// predicate re-checks so both the relay skip gate and the text-side fail-closed gates
/// share one truth table:
///
/// | analyzer | kinds            | result |
/// |----------|------------------|--------|
/// | 1        | _                | false  |
/// | 2        | [Japanese]       | true   |
/// | 2        | _                | false  |
/// | 3        | [Korean]         | true   |
/// | 3        | _                | false  |
/// | 0        | []               | false  |
/// | 0        | non-empty subset | true   |
/// | other    | _                | false  |
///
/// In particular (0, []) is false: a dictionary-less koine index provisions with zero
/// dict calls and stays Ready on the bigram fallback. Unknown ids are NOT treated as
/// dictionary-carrying; admission validation of the analyzer id set stays with the Router.
pub const fn dict_required(analyzer_id: u32, kinds: &[DictKind]) -> bool {
    match analyzer_id {
        1 => false,
        2 => matches!(kinds, [DictKind::Japanese]),
        3 => matches!(kinds, [DictKind::Korean]),
        0 => !kinds.is_empty(),
        _ => false,
    }
}

/// Per-frame metadata for one `admin_upload_dict_chunk` call in framed streaming mode
/// (plan 0342). `None` on the wire selects the existing raw mode (byte-identical behavior);
/// `Some` selects framed mode. The catalog artifact is N independent zstd frames; frame i
/// travels as relay call i and is decoded immediately on arrival (no compressed staging).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CompressedDictUpload {
    /// xxh3_128 over THIS call's frame bytes, verified before the receiver decodes.
    pub frame_digest: u128,
    /// Declared total RAW container length (bomb gate: the receiver enforces cumulative
    /// raw offset + frame content size ≤ raw_len on every call).
    pub raw_len: u64,
}

/// Framed-mode metadata for one `admin_finalize_dict_upload` call. The per-frame digests
/// replaced the former whole-stream compressed digest: the accumulated RAW digest over
/// region 16 is the final authority, re-verified here against the pinned catalog metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct CompressedDictFinalize {
    /// xxh3_128 over the RAW container (pinned as content identity in the text canister).
    pub raw_digest: u128,
    /// Expected decompressed (raw) container length.
    pub raw_len: u64,
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

/// Read-only dictionary status (`admin_get_dict_status`). While Uploading, `len` is the
/// accumulated RAW offset (framed streaming progress: each decoded frame extends it) and
/// `digest` is `None`; finalize pins both the raw digest and the final length.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DictStatus {
    pub state: DictState,
    /// xxh3_128 over the raw container bytes; `None` before finalize.
    pub digest: Option<u128>,
    /// Raw container byte length (accumulated raw offset while Uploading).
    pub len: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{decode_args, encode_args};

    #[test]
    fn dict_required_boundaries() {
        use DictKind::{Japanese as J, Korean as K};
        // Analyzer 1 never; unknown ids never.
        for kinds in [&[][..], &[J][..], &[K][..], &[J, K][..]] {
            assert!(!dict_required(1, kinds));
            assert!(!dict_required(4, kinds));
            assert!(!dict_required(u32::MAX, kinds));
        }
        // Analyzer 2 requires exactly {Japanese}; 3 exactly {Korean}.
        assert!(dict_required(2, &[J]));
        assert!(!dict_required(2, &[]));
        assert!(!dict_required(2, &[K]));
        assert!(!dict_required(2, &[J, K]));
        assert!(dict_required(3, &[K]));
        assert!(!dict_required(3, &[]));
        assert!(!dict_required(3, &[J]));
        assert!(!dict_required(3, &[J, K]));
        // Analyzer 0: any non-empty subset; empty means dictionary-less (plan 0343).
        assert!(!dict_required(0, &[]));
        assert!(dict_required(0, &[J]));
        assert!(dict_required(0, &[K]));
        assert!(dict_required(0, &[J, K]));
    }

    #[test]
    fn dict_kind_candid_roundtrip_canonical_order() {
        let mut kinds = vec![DictKind::Korean, DictKind::Japanese];
        kinds.sort();
        assert_eq!(kinds, vec![DictKind::Japanese, DictKind::Korean]);
        let bytes = encode_args((kinds.clone(),)).unwrap();
        let decoded: (Vec<DictKind>,) = decode_args(&bytes).unwrap();
        assert_eq!(decoded.0, kinds);
    }

    #[test]
    fn framed_types_candid_roundtrip() {
        let upload = CompressedDictUpload {
            frame_digest: u128::MAX,
            raw_len: 52_930_923,
        };
        let bytes = encode_args((upload.clone(),)).unwrap();
        let decoded: (CompressedDictUpload,) = decode_args(&bytes).unwrap();
        assert_eq!(decoded.0, upload);

        let finalize = CompressedDictFinalize {
            raw_digest: 0xDEADBEEF,
            raw_len: 52_930_923,
        };
        let bytes = encode_args((finalize.clone(),)).unwrap();
        let decoded: (CompressedDictFinalize,) = decode_args(&bytes).unwrap();
        assert_eq!(decoded.0, finalize);
    }

    #[test]
    fn dict_status_candid_roundtrip_all_states() {
        for (state, digest) in [
            (DictState::Absent, None),
            (DictState::Uploading, None),
            (DictState::Finalized, Some(u128::MAX)),
        ] {
            let status = DictStatus {
                state,
                digest,
                len: 52_930_923,
            };
            let bytes = encode_args((status.clone(),)).unwrap();
            let decoded: (DictStatus,) = decode_args(&bytes).unwrap();
            assert_eq!(decoded.0, status);
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
                frame_digest: u128::MAX,
                raw_len: 52_930_923,
            }),
        );
        let bytes = encode_args(args).unwrap();
        assert!(
            bytes.len() <= 2 * 1024 * 1024,
            "max compressed chunk + metadata encoded to {} bytes, exceeding 2 MiB",
            bytes.len()
        );
    }
}
