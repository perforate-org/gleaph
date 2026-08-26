//! Local textual forms: SHA-256 hex, canister-kind names, identity labels.
//!
//! These are operator-UI concerns only — the wire always carries raw bytes and the neutral
//! candid enums; nothing here participates in encoding.

use gleaph_artifact_api::types::{ArtifactId, CanisterKind};

/// Lowercase hex of `bytes`.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parse a 64-character hex string (case-insensitive) into a SHA-256 digest.
pub fn parse_sha256_hex(text: &str) -> Result<[u8; 32], String> {
    let text = text.trim();
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "sha256 must be exactly 64 hex characters, got {text:?}"
        ));
    }
    let mut digest = [0u8; 32];
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|error| format!("invalid sha256 hex: {error}"))?;
    }
    Ok(digest)
}

/// Parse an arbitrary even-length hex string (case-insensitive) into bytes; empty input is an
/// empty blob.
pub fn parse_hex_blob(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("expected an even-length hex string, got {text:?}"));
    }
    (0..text.len() / 2)
        .map(|index| {
            u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
                .map_err(|error| format!("invalid hex: {error}"))
        })
        .collect()
}

/// The canonical did variant name of a kind (also accepted by `--kind`).
pub fn kind_name(kind: CanisterKind) -> &'static str {
    match kind {
        CanisterKind::Router => "Router",
        CanisterKind::Graph => "Graph",
        CanisterKind::PropertyIndex => "PropertyIndex",
        CanisterKind::VectorCanister => "VectorCanister",
        CanisterKind::TextCanister => "TextCanister",
    }
}

/// Parse a `--kind` value; only the exact did variant names are accepted so an operator can
/// paste them straight from the candid file.
pub fn parse_kind(name: &str) -> Result<CanisterKind, String> {
    match name {
        "Router" => Ok(CanisterKind::Router),
        "Graph" => Ok(CanisterKind::Graph),
        "PropertyIndex" => Ok(CanisterKind::PropertyIndex),
        "VectorCanister" => Ok(CanisterKind::VectorCanister),
        "TextCanister" => Ok(CanisterKind::TextCanister),
        other => Err(format!(
            "unknown kind {other:?}; expected one of Router, Graph, PropertyIndex, VectorCanister, TextCanister"
        )),
    }
}

/// One-line human label of an artifact id, e.g. `Router 1.2.3 sha256:<hex>`.
pub fn artifact_id_label(id: &ArtifactId) -> String {
    format!(
        "{} {} sha256:{}",
        kind_name(id.canister_kind),
        id.semantic_version,
        to_hex(&id.sha256)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn hex_round_trips_and_accepts_uppercase() {
        let digest = parse_sha256_hex(ABC_HEX).expect("lowercase");
        assert_eq!(to_hex(&digest), ABC_HEX);
        let upper = parse_sha256_hex(&ABC_HEX.to_uppercase()).expect("uppercase");
        assert_eq!(upper, digest);
    }

    #[test]
    fn hex_rejects_wrong_length_and_non_hex() {
        assert!(parse_sha256_hex("").is_err());
        assert!(parse_sha256_hex("ab").is_err());
        assert!(parse_sha256_hex(&"g".repeat(64)).is_err());
        assert!(parse_sha256_hex(&"a".repeat(63)).is_err());
        assert!(parse_sha256_hex(&"a".repeat(65)).is_err());
    }

    #[test]
    fn hex_blob_parses_pairs_and_rejects_odd_input() {
        assert_eq!(parse_hex_blob("").expect("empty"), Vec::<u8>::new());
        assert_eq!(parse_hex_blob("00ff").expect("pairs"), vec![0x00, 0xff]);
        assert_eq!(
            parse_hex_blob("AbCd").expect("mixed case"),
            vec![0xab, 0xcd]
        );
        assert!(parse_hex_blob("0").is_err(), "odd length");
        assert!(parse_hex_blob("zz").is_err(), "non-hex");
    }

    #[test]
    fn kinds_parse_only_exact_did_names() {
        for name in [
            "Router",
            "Graph",
            "PropertyIndex",
            "VectorCanister",
            "TextCanister",
        ] {
            assert_eq!(kind_name(parse_kind(name).expect("parse")), name);
        }
        for bad in ["router", "text", "Provision", "", "TEXT_CANISTER"] {
            let error = parse_kind(bad).expect_err("must reject");
            assert!(error.contains("unknown kind"), "got: {error}");
        }
    }

    #[test]
    fn identity_label_renders_kind_version_digest() {
        let id = ArtifactId::new(
            CanisterKind::Graph,
            "1.2.3".to_owned(),
            parse_sha256_hex(ABC_HEX).expect("digest"),
        );
        assert_eq!(
            artifact_id_label(&id),
            format!("Graph 1.2.3 sha256:{ABC_HEX}")
        );
    }
}
